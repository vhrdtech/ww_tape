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
    pub const ALL: [Scalar; 11] = [
        Scalar::I8,
        Scalar::I16,
        Scalar::I32,
        Scalar::I64,
        Scalar::U8,
        Scalar::U16,
        Scalar::U32,
        Scalar::U64,
        Scalar::F32,
        Scalar::F64,
        Scalar::Bool,
    ];

    /// The scalar's code: 0 `I8` to 9 `F64`, 12 `Bool`. Codes 10 and 11 are complex elements, which are no scalar.
    pub const fn code(self) -> u8 {
        match self {
            Scalar::I8 => 0,
            Scalar::I16 => 1,
            Scalar::I32 => 2,
            Scalar::I64 => 3,
            Scalar::U8 => 4,
            Scalar::U16 => 5,
            Scalar::U32 => 6,
            Scalar::U64 => 7,
            Scalar::F32 => 8,
            Scalar::F64 => 9,
            Scalar::Bool => 12,
        }
    }

    /// The scalar of `code`, None for a code that names no scalar.
    pub const fn from_code(code: u8) -> Option<Scalar> {
        let s = match code {
            0 => Scalar::I8,
            1 => Scalar::I16,
            2 => Scalar::I32,
            3 => Scalar::I64,
            4 => Scalar::U8,
            5 => Scalar::U16,
            6 => Scalar::U32,
            7 => Scalar::U64,
            8 => Scalar::F32,
            9 => Scalar::F64,
            12 => Scalar::Bool,
            _ => return None,
        };
        Some(s)
    }

    /// Bits of one value.
    pub const fn bits(self) -> u32 {
        match self {
            Scalar::Bool => 1,
            Scalar::I8 | Scalar::U8 => 8,
            Scalar::I16 | Scalar::U16 => 16,
            Scalar::I32 | Scalar::U32 | Scalar::F32 => 32,
            Scalar::I64 | Scalar::U64 | Scalar::F64 => 64,
        }
    }

    /// Whether it is a signed integer.
    pub const fn is_signed(self) -> bool {
        matches!(self, Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64)
    }

    /// Whether it is a float.
    pub const fn is_float(self) -> bool {
        matches!(self, Scalar::F32 | Scalar::F64)
    }
}

impl core::fmt::Display for Scalar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Scalar::I8 => "i8",
            Scalar::I16 => "i16",
            Scalar::I32 => "i32",
            Scalar::I64 => "i64",
            Scalar::U8 => "u8",
            Scalar::U16 => "u16",
            Scalar::U32 => "u32",
            Scalar::U64 => "u64",
            Scalar::F32 => "f32",
            Scalar::F64 => "f64",
            Scalar::Bool => "bool",
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
