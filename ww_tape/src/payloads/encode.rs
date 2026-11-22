//! Encoding a chunk's payloads into its two columns.

use super::LENGTHS;
use crate::chunk::{Codec, ColumnKind, ColumnSpec};
use crate::lz4::{LZ4_DECODED_MAX, Lz4Error, lz4_encode, lz4_max_len};
use alloc::borrow::Cow;
use alloc::vec;
use alloc::vec::Vec;
use ww_tape_codecs::lengths::encode_lengths;

/// Why payloads can't be encoded.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeError {
    /// No items.
    Empty,
    /// More items than a chunk holds (u32).
    TooMany,
    /// The payloads add up to more than [LZ4_DECODED_MAX] bytes.
    TooLong { len: usize },
    /// The lz4 encoder refused the payloads.
    Lz4(Lz4Error),
    /// The lengths don't add up to the payload bytes.
    Lengths,
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "no items"),
            Self::TooMany => write!(f, "more items than a chunk holds"),
            Self::TooLong { len } => {
                write!(f, "{len} bytes of payloads, over {LZ4_DECODED_MAX}")
            }
            Self::Lz4(_) => write!(f, "lz4 encoder refused the payloads"),
            Self::Lengths => write!(f, "lengths don't add up to the payload bytes"),
        }
    }
}

impl core::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Lz4(e) => Some(e),
            _ => None,
        }
    }
}

/// A chunk's payloads encoded: the lengths column and the payloads column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedPayloads {
    lengths: Vec<u8>,
    payloads: Vec<u8>,
    codec: Codec,
}

impl EncodedPayloads {
    /// Encode the payloads of a chunk's items, in item order. The payloads column is [Codec::LZ4] when that is
    /// smaller than the bytes as they are, else [Codec::RAW]. The same payloads give the same bytes (lz4: see
    /// [crate::lz4]).
    pub fn encode<'a>(items: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, EncodeError> {
        let mut lens: Vec<u32> = Vec::new();
        let mut raw: Vec<u8> = Vec::new();
        for item in items {
            let len = raw.len().saturating_add(item.len());
            if len > LZ4_DECODED_MAX {
                return Err(EncodeError::TooLong { len });
            }
            // the count stays a u32
            if !u32::try_from(lens.len()).is_ok_and(|n| n < u32::MAX) {
                return Err(EncodeError::TooMany);
            }
            // at most the running total, at most LZ4_DECODED_MAX
            let Ok(item_len) = u32::try_from(item.len()) else {
                return Err(EncodeError::TooLong { len });
            };
            lens.push(item_len);
            raw.extend_from_slice(item);
        }
        Self::pack(&lens, Cow::Owned(raw), Vec::new())
    }

    /// Encode payloads given as their lengths in item order and their bytes one after another, as [Self::encode]
    /// does: the same bytes. The payloads column goes into `out`, a buffer to reuse (its bytes are dropped). Refused
    /// when the lengths don't add up to the bytes.
    pub fn from_parts(lens: &[u32], raw: &[u8], out: Vec<u8>) -> Result<Self, EncodeError> {
        if raw.len() > LZ4_DECODED_MAX {
            return Err(EncodeError::TooLong { len: raw.len() });
        }
        if !u32::try_from(lens.len()).is_ok_and(|n| n < u32::MAX) {
            return Err(EncodeError::TooMany);
        }
        let sum = lens
            .iter()
            .try_fold(0usize, |sum, &l| sum.checked_add(usize::try_from(l).ok()?));
        if sum != Some(raw.len()) {
            return Err(EncodeError::Lengths);
        }
        Self::pack(lens, Cow::Borrowed(raw), out)
    }

    /// Encode lengths `lens` and payload bytes `raw` checked before, the payloads column into `out`.
    fn pack(lens: &[u32], raw: Cow<'_, [u8]>, mut out: Vec<u8>) -> Result<Self, EncodeError> {
        if lens.is_empty() {
            return Err(EncodeError::Empty);
        }
        let lengths = encode_lengths(lens).ok_or(EncodeError::TooLong { len: raw.len() })?;
        let max = lz4_max_len(raw.len());
        if out.capacity() < max {
            // a fresh buffer comes zeroed from the allocator
            out = vec![0; max];
        } else {
            out.clear();
            out.resize(max, 0);
        }
        let n = lz4_encode(&raw, &mut out).map_err(EncodeError::Lz4)?;
        let (payloads, codec) = if n < raw.len() {
            out.truncate(n);
            (out, Codec::LZ4)
        } else {
            match raw {
                Cow::Owned(raw) => (raw, Codec::RAW),
                Cow::Borrowed(raw) => {
                    out.clear();
                    out.extend_from_slice(raw);
                    (out, Codec::RAW)
                }
            }
        };
        Ok(Self {
            lengths,
            payloads,
            codec,
        })
    }

    /// The codec of the payloads column: [Codec::RAW] or [Codec::LZ4].
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// The two columns with their bytes, taken: lengths, then payloads.
    pub fn into_columns(self) -> [(ColumnSpec, Vec<u8>); 2] {
        let lengths = ColumnSpec {
            kind: ColumnKind::PayloadLengths,
            codec: LENGTHS,
            param: 0,
            len: self.lengths.len(),
        };
        let payloads = ColumnSpec {
            kind: ColumnKind::Payloads,
            codec: self.codec,
            param: 0,
            len: self.payloads.len(),
        };
        [(lengths, self.lengths), (payloads, self.payloads)]
    }

    /// The two columns with their bytes: lengths, then payloads.
    pub fn columns(&self) -> [(ColumnSpec, &[u8]); 2] {
        let spec = |kind, codec, bytes: &[u8]| ColumnSpec {
            kind,
            codec,
            param: 0,
            len: bytes.len(),
        };
        [
            (
                spec(ColumnKind::PayloadLengths, LENGTHS, &self.lengths),
                &self.lengths,
            ),
            (
                spec(ColumnKind::Payloads, self.codec, &self.payloads),
                &self.payloads,
            ),
        ]
    }
}
