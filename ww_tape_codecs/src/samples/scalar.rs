//! The scalar types a sample codec codes.

/// The type of one value a sample codec codes: an integer, a float or a bit.
///
/// Its code ([Scalar::code]) is the element code the format gives the same type, so an element whose scalar it is
/// maps to it and back by the code alone.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scalar {
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
    /// A bit; stored 8 per byte, the first in bit 0.
    Bool,
}

impl Scalar {
    /// Every scalar, by code.
    pub const ALL: [Self; 11] = [
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
        Self::Bool,
    ];

    /// The scalar's code: 0 `I8` to 9 `F64`, 12 `Bool`. Codes 10 and 11 are complex elements, which are no scalar.
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
            Self::Bool => 12,
        }
    }

    /// The scalar of `code`, None for a code that names no scalar.
    pub const fn from_code(code: u8) -> Option<Self> {
        let s = match code {
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
            12 => Self::Bool,
            _ => return None,
        };
        Some(s)
    }

    /// Bits of one value.
    pub const fn bits(self) -> u32 {
        match self {
            Self::Bool => 1,
            Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 | Self::F32 => 32,
            Self::I64 | Self::U64 | Self::F64 => 64,
        }
    }

    /// Whether it is a signed integer.
    pub const fn is_signed(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    /// Whether it is a float.
    pub const fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }
}

impl core::fmt::Display for Scalar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_name_one_scalar_each() {
        for s in Scalar::ALL {
            assert_eq!(Scalar::from_code(s.code()), Some(s));
        }
        for code in [10, 11, 13, 255] {
            assert_eq!(Scalar::from_code(code), None);
        }
    }
}
