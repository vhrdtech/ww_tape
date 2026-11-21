//! What a sample is: its element type and channels.

use shrink_wrap::prelude::*;

use super::{Lane, Scalar};

/// A predefined element: a scalar, or a complex pair of one.
///
/// Its [code](Element::code) is what a summary head names its element by when it is one of these; any other
/// element is described in full ([ElementDesc]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Element {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// An I/Q pair of `i16`, I first.
    ComplexI16,
    /// An I/Q pair of `f32`, I first.
    ComplexF32,
    /// A bit; stored 8 per byte, the first in bit 0.
    Bool,
}

impl Element {
    /// The code of a summary head's element described in full ([ElementDesc]), no element's code.
    pub const DESCRIBED_CODE: u8 = 13;

    /// Every element, by code.
    pub const ALL: [Self; 13] = [
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::U8,
        Self::U16,
        Self::U32,
        Self::U64,
        Self::F32,
        Self::F64,
        Self::ComplexI16,
        Self::ComplexF32,
        Self::Bool,
    ];

    /// The element code: 0 `I8` to 12 `Bool`, in declaration order.
    pub const fn code(self) -> u8 {
        match self {
            Self::I8 => 0,
            Self::I16 => 1,
            Self::I32 => 2,
            Self::I64 => 3,
            Self::U8 => 4,
            Self::U16 => 5,
            Self::U32 => 6,
            Self::U64 => 7,
            Self::F32 => 8,
            Self::F64 => 9,
            Self::ComplexI16 => 10,
            Self::ComplexF32 => 11,
            Self::Bool => 12,
        }
    }

    /// The element of a code this reader knows, None for any other.
    pub const fn from_code(code: u8) -> Option<Self> {
        let e = match code {
            0 => Self::I8,
            1 => Self::I16,
            2 => Self::I32,
            3 => Self::I64,
            4 => Self::U8,
            5 => Self::U16,
            6 => Self::U32,
            7 => Self::U64,
            8 => Self::F32,
            9 => Self::F64,
            10 => Self::ComplexI16,
            11 => Self::ComplexF32,
            12 => Self::Bool,
            _ => return None,
        };
        Some(e)
    }

    /// The type a value is stored as: the part of a complex element, else the element itself.
    pub const fn scalar(self) -> Self {
        match self {
            Self::ComplexI16 => Self::I16,
            Self::ComplexF32 => Self::F32,
            other => other,
        }
    }

    /// The element of scalar `s`, with its code.
    pub const fn of_scalar(s: Scalar) -> Self {
        match s {
            Scalar::I8 => Self::I8,
            Scalar::I16 => Self::I16,
            Scalar::I32 => Self::I32,
            Scalar::I64 => Self::I64,
            Scalar::U8 => Self::U8,
            Scalar::U16 => Self::U16,
            Scalar::U32 => Self::U32,
            Scalar::U64 => Self::U64,
            Scalar::F32 => Self::F32,
            Scalar::F64 => Self::F64,
            Scalar::Bool => Self::Bool,
        }
    }

    /// The scalar a value of the element is stored as: the part of a complex element.
    pub const fn scalar_type(self) -> Scalar {
        match self {
            Self::I8 => Scalar::I8,
            Self::I16 | Self::ComplexI16 => Scalar::I16,
            Self::I32 => Scalar::I32,
            Self::I64 => Scalar::I64,
            Self::U8 => Scalar::U8,
            Self::U16 => Scalar::U16,
            Self::U32 => Scalar::U32,
            Self::U64 => Scalar::U64,
            Self::F32 | Self::ComplexF32 => Scalar::F32,
            Self::F64 => Scalar::F64,
            Self::Bool => Scalar::Bool,
        }
    }

    /// Scalars per element: 2 for a complex element, else 1.
    pub const fn parts(self) -> u32 {
        match self {
            Self::ComplexI16 | Self::ComplexF32 => 2,
            _ => 1,
        }
    }

    /// Bits of one scalar of the element.
    pub const fn scalar_bits(self) -> u32 {
        self.scalar_type().bits()
    }

    /// Whether the scalar is a signed integer.
    pub const fn is_signed(self) -> bool {
        self.scalar_type().is_signed()
    }

    /// Whether the scalar is a float.
    pub const fn is_float(self) -> bool {
        self.scalar_type().is_float()
    }
}

impl core::fmt::Display for Element {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = self.scalar_type();
        match self {
            Self::ComplexI16 | Self::ComplexF32 => write!(f, "complex {s}"),
            _ => write!(f, "{s}"),
        }
    }
}

/// Most parts of an [ElementDesc]: a value of up to 16 numbers.
pub const ELEMENT_PARTS_MAX: u32 = 16;

/// One element described by data: what a stored number is, whatever device sent it.
///
/// 8-bit IQ (`ComplexI8`, `ComplexU8` offset by 127.5), a packed 12-bit device format, bfloat16: each is a
/// description, no code change. Every [Element] is one too ([ElementDesc::of]). A part is read into its
/// [scalar](Self::scalar) and coded by the [raw codec](crate::Codec::RAW) as the [lane](Self::lane) says.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, Clone, Copy, PartialEq, Eq, Hash)
)]
pub struct ElementDesc {
    /// Stored bits of one part, 1 to 64.
    pub bits: u8,
    /// 0: an integer. Else a float of this many exponent bits, the sign one bit and the mantissa the rest: f16 5,
    /// bfloat16 8, f32 8, f64 11.
    pub exponent_bits: u8,
    /// Whether an integer is signed (two's complement); a float is always signed.
    pub signed: bool,
    /// How the raw codec stores a part: a whole-byte width big-endian, any other width from the top bit of the
    /// first byte. False: little-endian, from bit 0.
    pub msb_first: bool,
    /// Numbers per element, 1 to [ELEMENT_PARTS_MAX]: 2 for an I/Q pair, I first.
    pub parts: UNib32,
    /// The stored number that stands for zero: `255/2` for 8-bit IQ offset to 127.5. None: 0. Only for an integer
    /// of at most 32 bits; its summaries hold `q · value - p` for a zero `p/q` ([Self::summary_element]).
    #[default = None]
    pub zero: Option<exact_units::Ratio64>,
}

impl ElementDesc {
    /// The description of `element`.
    pub const fn of(element: Element) -> Self {
        Self::of_scalar(element.scalar_type(), element.parts())
    }

    /// `parts` numbers of scalar `s`, each stored whole, little-endian.
    pub const fn of_scalar(s: Scalar, parts: u32) -> Self {
        let bits = s.bits();
        let exponent_bits = match (s.is_float(), bits) {
            (true, 32) => 8,
            (true, _) => 11,
            (false, _) => 0,
        };
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "a scalar has at most 64 bits"
        )]
        let bits = bits as u8;
        Self {
            bits,
            exponent_bits,
            signed: s.is_signed() || s.is_float(),
            msb_first: false,
            parts: UNib32(parts),
            zero: None,
        }
    }

    /// The predefined [Element] this describes, None for any other description.
    pub fn element(&self) -> Option<Element> {
        Element::ALL.into_iter().find(|e| Self::of(*e) == *self)
    }

    /// The scalar a part is read into: a 1-bit unsigned integer is a `bool`; an integer the smallest of 8, 16, 32
    /// and 64 bits that holds it, of its sign; a float `f32` when f32 holds every value of it exactly (at most 8
    /// exponent and 23 mantissa bits), else `f64`.
    pub const fn scalar(&self) -> Scalar {
        match (self.exponent_bits, self.bits, self.signed) {
            (0, 1, false) => Scalar::Bool,
            (0, 0..=8, true) => Scalar::I8,
            (0, 0..=8, false) => Scalar::U8,
            (0, 9..=16, true) => Scalar::I16,
            (0, 9..=16, false) => Scalar::U16,
            (0, 17..=32, true) => Scalar::I32,
            (0, 17..=32, false) => Scalar::U32,
            (0, _, true) => Scalar::I64,
            (0, _, false) => Scalar::U64,
            (e, b, _) => {
                let mantissa = b.saturating_sub(1).saturating_sub(e);
                if e <= 8 && mantissa <= 23 {
                    Scalar::F32
                } else {
                    Scalar::F64
                }
            }
        }
    }

    /// Bits of the integer or float a part is read into ([Self::scalar]), 8 for a `bool`.
    pub const fn container_bits(&self) -> u32 {
        match self.scalar() {
            Scalar::Bool => 8,
            s => s.bits(),
        }
    }

    /// How the raw codec stores one part: its scalar, bits, bit order and, for a float of another layout than its
    /// scalar's, its exponent bits.
    pub fn lane(&self) -> Lane {
        let scalar = self.scalar();
        let bits = u32::from(self.bits);
        let own_float = scalar.is_float() && bits == scalar.bits();
        Lane {
            scalar,
            bits,
            msb_first: self.msb_first,
            exponent_bits: if own_float {
                0
            } else {
                u32::from(self.exponent_bits)
            },
        }
    }

    /// The stored values an integer part can hold, lowest and highest; None for a float or a width past 64 bits.
    pub fn range(&self) -> Option<(i128, i128)> {
        if self.exponent_bits != 0 || self.bits == 0 || self.bits > 64 {
            return None;
        }
        let bits = u32::from(self.bits);
        if self.signed {
            let half = 1i128.checked_shl(bits.checked_sub(1)?)?;
            Some((half.checked_neg()?, half.checked_sub(1)?))
        } else {
            Some((0, 1i128.checked_shl(bits)?.checked_sub(1)?))
        }
    }

    /// Whether the description is one a writer takes: 1 to 64 bits; a float signed, with 2 to 11 exponent bits and
    /// at least one mantissa bit, of at most 32 bits or the f64 layout, and no zero; 1 to [ELEMENT_PARTS_MAX] parts;
    /// a zero only on an integer of at most 32 bits, and such that every stored value shifted by it fits the
    /// [summary element](Self::summary_element).
    pub fn is_valid(&self) -> bool {
        if !(1..=64).contains(&self.bits) || !(1..=ELEMENT_PARTS_MAX).contains(&self.parts.0) {
            return false;
        }
        let e = u32::from(self.exponent_bits);
        let bits = u32::from(self.bits);
        if e != 0 {
            return self.signed
                && (2..=11).contains(&e)
                && e.saturating_add(2) <= bits
                && (bits <= 32 || (bits == 64 && e == 11))
                && self.zero.is_none();
        }
        let Some((p, q)) = self.zero_shift() else {
            return true;
        };
        let Some((low, high)) = self.range() else {
            return false;
        };
        if self.bits > 32 || self.scalar() == Scalar::Bool {
            return false;
        }
        // the summary element: twice the container, signed
        let (low_fit, high_fit) = match self.container_bits() {
            8 => (i128::from(i16::MIN), i128::from(i16::MAX)),
            16 => (i128::from(i32::MIN), i128::from(i32::MAX)),
            _ => (i128::from(i64::MIN), i128::from(i64::MAX)),
        };
        let fits = |v: i128| {
            q.checked_mul(v)
                .and_then(|x| x.checked_sub(p))
                .is_some_and(|x| (low_fit..=high_fit).contains(&x))
        };
        fits(low) && fits(high)
    }

    /// Whether the [raw codec](crate::Codec::RAW) codes a part: a [valid](Self::is_valid) description but an
    /// MSB-first integer of more than 56 bits that doesn't fill its scalar.
    pub fn is_coded(&self) -> bool {
        self.is_valid() && ww_tape_codecs::samples::raw_codes(self.lane())
    }

    /// The element a summary of this one is kept in: the same numbers, bit order aside, or for an element with a
    /// [zero](Self::zero) `p/q` the signed integer of twice the container's bits that holds `q · value - p`.
    pub const fn summary_element(&self) -> Self {
        let mut e = *self;
        e.msb_first = false;
        if e.zero.is_some() {
            e.bits = match self.container_bits() {
                8 => 16,
                16 => 32,
                _ => 64,
            };
            e.signed = true;
            e.zero = None;
        }
        e
    }

    /// The [zero](Self::zero) as `(p, q)`: a stored value `v` means `v - p/q`, so its summaries hold `q · v - p`.
    pub fn zero_shift(&self) -> Option<(i128, i128)> {
        let (p, q) = self.zero?.parts();
        Some((i128::from(p), i128::from(q)))
    }
}

impl core::fmt::Display for ElementDesc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(e) = self.element() {
            return write!(f, "{e}");
        }
        if self.parts.0 != 1 {
            write!(f, "{} × ", self.parts.0)?;
        }
        if self.exponent_bits != 0 {
            write!(f, "f{}e{}", self.bits, self.exponent_bits)?;
        } else {
            write!(f, "{}{}", if self.signed { 'i' } else { 'u' }, self.bits)?;
        }
        if self.msb_first {
            f.write_str(" msb")?;
        }
        if let Some((p, q)) = self.zero_shift() {
            write!(f, " zero {p}/{q}")?;
        }
        Ok(())
    }
}

/// What one sample of a series column is: `channels` elements, interleaved.
///
/// A column holds one channel or several interleaved; the [Codec](crate::Codec) of the column says how the values
/// are coded, apart from this. A track's shape is its [Items::Numbers](crate::Items::Numbers) element and its
/// [SeriesDef::channels](crate::SeriesDef::channels) ([TrackDef::series_shape](crate::TrackDef::series_shape)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SampleShape {
    pub element: ElementDesc,
    /// Elements per sample, 1 and up.
    pub channels: u32,
}

impl SampleShape {
    /// One channel of `element`.
    pub const fn scalar(element: ElementDesc) -> Self {
        Self {
            element,
            channels: 1,
        }
    }

    /// `channels` channels of `element`.
    pub const fn new(element: ElementDesc, channels: u32) -> Self {
        Self { element, channels }
    }

    /// Scalars per sample (channels times [ElementDesc::parts]), None when 0 or past u32.
    pub const fn lanes(&self) -> Option<u32> {
        match self.channels.checked_mul(self.element.parts.0) {
            Some(0) | None => None,
            Some(n) => Some(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_declaration_order() {
        for (i, e) in Element::ALL.iter().enumerate() {
            assert_eq!(e.code() as usize, i);
            assert_eq!(Element::from_code(i as u8), Some(*e));
        }
        assert_eq!(Element::from_code(Element::DESCRIBED_CODE), None);
        assert_eq!(Element::from_code(255), None);
    }

    /// Every scalar is an element of its code and back; every element has its scalar and its description.
    #[test]
    fn scalars_and_elements_map_both_ways() {
        for s in Scalar::ALL {
            let e = Element::of_scalar(s);
            assert_eq!(e.code(), s.code());
            assert_eq!(e.scalar_type(), s);
        }
        for e in Element::ALL {
            let s = e.scalar_type();
            assert_eq!(s.code(), e.scalar().code(), "{e:?}");
            assert_eq!(Element::of_scalar(s), e.scalar());
            let d = ElementDesc::of(e);
            assert_eq!(d.element(), Some(e), "{e:?}");
            assert_eq!(d.scalar(), s, "{e:?}");
            assert_eq!(d.lane(), Lane::of(s), "{e:?}");
            assert!(d.is_valid() && d.is_coded(), "{e:?}");
            assert_eq!(d.summary_element(), d, "{e:?}");
            assert_eq!(d.to_string(), e.to_string());
        }
        let shape = SampleShape::new(ElementDesc::of(Element::ComplexI16), 3);
        assert_eq!(shape.lanes(), Some(6));
        assert_eq!(
            SampleShape::new(ElementDesc::of(Element::F32), 0).lanes(),
            None
        );
    }

    fn int(bits: u8, signed: bool) -> ElementDesc {
        ElementDesc {
            bits,
            exponent_bits: 0,
            signed,
            msb_first: false,
            parts: UNib32(1),
            zero: None,
        }
    }

    fn float(bits: u8, exponent_bits: u8) -> ElementDesc {
        ElementDesc {
            bits,
            exponent_bits,
            signed: true,
            msb_first: false,
            parts: UNib32(1),
            zero: None,
        }
    }

    /// A described element is read into the smallest scalar that holds it and coded at its own width.
    #[test]
    fn described_elements_pick_their_scalar() {
        assert_eq!(int(12, false).scalar(), Scalar::U16);
        assert_eq!(int(12, true).scalar(), Scalar::I16);
        assert_eq!(int(1, false).scalar(), Scalar::Bool);
        assert_eq!(int(1, true).scalar(), Scalar::I8);
        assert_eq!(int(24, true).scalar(), Scalar::I32);
        assert_eq!(int(40, false).scalar(), Scalar::U64);
        assert_eq!(float(16, 5).scalar(), Scalar::F32);
        assert_eq!(float(16, 8).scalar(), Scalar::F32);
        assert_eq!(float(8, 4).scalar(), Scalar::F32);
        assert_eq!(float(32, 9).scalar(), Scalar::F64);
        assert_eq!(float(64, 11).scalar(), Scalar::F64);
        let lane = int(12, false).lane();
        assert_eq!((lane.bits, lane.exponent_bits), (12, 0));
        assert_eq!(float(16, 8).lane().exponent_bits, 8);
        assert_eq!(float(32, 8).lane().exponent_bits, 0);
        assert_eq!(float(64, 11).lane().exponent_bits, 0);
        assert_eq!(int(12, false).to_string(), "u12");
        assert_eq!(float(16, 8).to_string(), "f16e8");
        let mut iq = int(8, false);
        iq.parts = UNib32(2);
        iq.zero = exact_units::Ratio64::new(255, 2);
        assert_eq!(iq.to_string(), "2 × u8 zero 255/2");
        let mut msb = int(24, true);
        msb.msb_first = true;
        assert_eq!(msb.to_string(), "i24 msb");
    }

    /// What a writer takes: the float layouts the codecs code, a zero only where its summaries fit.
    #[test]
    fn validity_follows_the_codecs() {
        assert!(int(12, false).is_coded());
        assert!(int(64, false).is_coded());
        assert!(!int(0, false).is_valid());
        assert!(!int(65, false).is_valid());
        let mut wide_msb = int(60, false);
        wide_msb.msb_first = true;
        assert!(wide_msb.is_valid() && !wide_msb.is_coded());
        let mut be = int(64, false);
        be.msb_first = true;
        assert!(be.is_coded());
        assert!(float(16, 5).is_coded());
        assert!(float(8, 4).is_coded());
        assert!(!float(8, 1).is_valid());
        assert!(!float(8, 7).is_valid());
        assert!(!float(48, 11).is_valid());
        assert!(!float(64, 10).is_valid());
        let mut unsigned = float(16, 5);
        unsigned.signed = false;
        assert!(!unsigned.is_valid());
        let mut parts = int(8, false);
        parts.parts = UNib32(ELEMENT_PARTS_MAX + 1);
        assert!(!parts.is_valid());
    }

    /// A zero sits only on an integer of at most 32 bits, and only where its shifted values fit the summary element.
    #[test]
    fn a_zero_fits_its_summary_element() {
        let mut iq = int(8, false);
        iq.zero = exact_units::Ratio64::new(255, 2);
        assert!(iq.is_valid());
        assert_eq!(iq.summary_element(), int(16, true));
        assert_eq!(iq.zero_shift(), Some((255, 2)));
        let mut too_far = int(8, false);
        too_far.zero = exact_units::Ratio64::new(1 << 16, 1);
        assert!(!too_far.is_valid());
        let mut wide = int(33, false);
        wide.zero = exact_units::Ratio64::int(1);
        assert!(!wide.is_valid());
        let mut bit = int(1, false);
        bit.zero = exact_units::Ratio64::int(1);
        assert!(!bit.is_valid());
        let mut f = float(16, 5);
        f.zero = exact_units::Ratio64::int(1);
        assert!(!f.is_valid());
        let mut signed = int(16, true);
        signed.zero = exact_units::Ratio64::new(1, 1 << 15);
        assert!(signed.is_valid());
        assert_eq!(signed.summary_element(), int(32, true));
    }
}
