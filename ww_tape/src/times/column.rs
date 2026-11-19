//! What every time and value column reader shares: the time mode of a starts column and the error of a column.

use crate::Codec;

/// How a starts column stores its ticks (a chunk head's time mode, a relation column's mode byte).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TimeMode {
    /// First tick plus a period per item.
    Implicit,
    /// A linear frame of reference per group of 128 items.
    Linear,
}

impl TimeMode {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(TimeMode::Implicit),
            1 => Some(TimeMode::Linear),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            TimeMode::Implicit => 0,
            TimeMode::Linear => 1,
        }
    }
}

/// Why a column of starts, durations, flags or values doesn't read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColumnError {
    /// The column breaks the format, as said.
    Corrupt(&'static str),
    /// A value column in a codec other than [Codec::STARTS] and [Codec::DURATIONS].
    UnknownCodec(Codec),
}

impl core::fmt::Display for ColumnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ColumnError::Corrupt(what) => f.write_str(what),
            ColumnError::UnknownCodec(c) => write!(f, "unknown codec {}", c.0),
        }
    }
}

impl core::error::Error for ColumnError {}
