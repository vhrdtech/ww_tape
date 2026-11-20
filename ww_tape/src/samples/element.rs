//! What a sample is: its element type and channels.

use shrink_wrap::prelude::*;

use super::Scalar;

/// The logical type of one value of a sample.
///
/// Its number (the enum's discriminant, [Element::code]) is the element code every block uses: summary heads name
/// their element by it.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, Clone, Copy, PartialEq, Eq, Hash),
    ww_repr = u8
)]
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
    /// Reserved, code 13: a sample that is a struct of leaves of different element types, the leaf list a later field
    /// of [SeriesDef](crate::SeriesDef). A reader that doesn't know it refuses the track ([SeriesDef::check](
    /// crate::SeriesDef::check)) and every column and summary of it, never misreads it.
    pub const LEAVES: Self = Self::Unknown(13);

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

    /// The element code: 0 `I8` to 12 `Bool`, in declaration order; a newer writer's element keeps its code. An
    /// unknown element past 255, which no byte names, gives 255, a code no element has.
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
            Self::Unknown(code) => match code.to_le_bytes() {
                [low, 0, 0, 0] => low,
                _ => u8::MAX,
            },
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

    /// The scalar a value of the element is stored as (the part of a complex element), None for an element this
    /// reader doesn't know.
    pub const fn scalar_type(self) -> Option<Scalar> {
        let s = match self.scalar() {
            Self::I8 => Scalar::I8,
            Self::I16 => Scalar::I16,
            Self::I32 => Scalar::I32,
            Self::I64 => Scalar::I64,
            Self::U8 => Scalar::U8,
            Self::U16 => Scalar::U16,
            Self::U32 => Scalar::U32,
            Self::U64 => Scalar::U64,
            Self::F32 => Scalar::F32,
            Self::F64 => Scalar::F64,
            Self::Bool => Scalar::Bool,
            _ => return None,
        };
        Some(s)
    }

    /// Scalars per element: 2 for a complex element, else 1.
    pub const fn parts(self) -> u32 {
        match self {
            Self::ComplexI16 | Self::ComplexF32 => 2,
            _ => 1,
        }
    }

    /// Bits of one scalar of the element, 0 for an element this reader doesn't know.
    pub const fn scalar_bits(self) -> u32 {
        match self.scalar_type() {
            Some(s) => s.bits(),
            None => 0,
        }
    }

    /// Whether the scalar is a signed integer.
    pub const fn is_signed(self) -> bool {
        matches!(self.scalar_type(), Some(s) if s.is_signed())
    }

    /// Whether the scalar is a float.
    pub const fn is_float(self) -> bool {
        matches!(self.scalar_type(), Some(s) if s.is_float())
    }
}

impl core::fmt::Display for Element {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match (self, self.scalar_type()) {
            (Self::ComplexI16 | Self::ComplexF32, Some(s)) => write!(f, "complex {s}"),
            (_, Some(s)) => write!(f, "{s}"),
            (_, None) => write!(f, "element {}", self.code()),
        }
    }
}

/// What one sample of a series column is: `channels` elements, interleaved.
///
/// A column holds one channel or several interleaved; the [Codec](crate::Codec) of the column says how the values
/// are coded, apart from this.
#[derive_shrink_wrap(
    borrowed,
    owned(feature = "std"),
    derive(Debug, Clone, Copy, PartialEq, Eq, Hash)
)]
pub struct SampleShape {
    pub element: Element,
    /// Elements per sample, 1 and up.
    pub channels: UNib32,
}

impl SampleShape {
    /// One channel of `element`.
    pub const fn scalar(element: Element) -> Self {
        Self {
            element,
            channels: UNib32(1),
        }
    }

    /// `channels` channels of `element`.
    pub const fn new(element: Element, channels: u32) -> Self {
        Self {
            element,
            channels: UNib32(channels),
        }
    }

    /// Scalars per sample (channels times [Element::parts]), None when 0 or past u32.
    pub const fn lanes(&self) -> Option<u32> {
        match self.channels.0.checked_mul(self.element.parts()) {
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
        assert_eq!(Element::from_code(13), None);
        assert_eq!(Element::Unknown(14).code(), 14);
        // past a byte: no known element's code, where truncation gave 256 the code of I8
        assert_eq!(Element::Unknown(256).code(), u8::MAX);
        assert_eq!(Element::from_code(Element::Unknown(269).code()), None);
    }

    /// Every scalar is an element of its code and back; every known element has a scalar of its scalar's code; an
    /// element this reader doesn't know has none.
    #[test]
    fn scalars_and_elements_map_both_ways() {
        for s in Scalar::ALL {
            let e = Element::of_scalar(s);
            assert_eq!(e.code(), s.code());
            assert_eq!(e.scalar_type(), Some(s));
        }
        for e in Element::ALL {
            let s = e.scalar_type().unwrap();
            assert_eq!(s.code(), e.scalar().code(), "{e:?}");
            assert_eq!(Element::of_scalar(s), e.scalar());
        }
        assert_eq!(Element::LEAVES.scalar_type(), None);
        assert_eq!(Element::Unknown(200).scalar_type(), None);
    }

    #[test]
    fn element_bytes_are_its_code() {
        // the element is a u8 discriminant on the wire, so its byte is the code
        for e in Element::ALL {
            let mut buf = [0u8; 8];
            let bytes = e.to_ww_bytes(&mut buf).unwrap();
            assert_eq!(bytes, [e.code()], "{e:?}");
        }
        let shape = SampleShape::new(Element::ComplexI16, 3);
        assert_eq!(shape.lanes(), Some(6));
        assert_eq!(SampleShape::new(Element::F32, 0).lanes(), None);
    }
}
