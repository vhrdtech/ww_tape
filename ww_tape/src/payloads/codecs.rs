//! The payload codecs: what a writer may choose for a payloads column and what a reader decodes.
//!
//! [Codec::RAW] is read in place and needs no entry. Every other payload codec is one [PayloadCoder] in [CODERS]: its
//! codec number, an encoder and a decoder. A codec added later is a new number and a new entry; files written before
//! read as they did, and a reader without the entry refuses only that column ([PayloadsError::UnknownCodec]).

use super::{EncodeError, PayloadsError};
use crate::chunk::Codec;
use crate::lz4::{lz4_decode, lz4_decoded_len, lz4_encode, lz4_max_len};
use alloc::vec;
use alloc::vec::Vec;

/// A payload codec other than [Codec::RAW]: its number, its encoder and its decoder.
#[derive(Copy, Clone, Debug)]
pub struct PayloadCoder {
    /// Its number in a column's codec byte.
    pub codec: Codec,
    /// Its name, as tools print it.
    pub name: &'static str,
    /// The bytes the encoder needs at most for so many raw bytes.
    pub(super) max_len: fn(usize) -> usize,
    /// Encode raw bytes into a buffer of `max_len` bytes, giving the bytes written.
    pub(super) encode: fn(&[u8], &mut [u8]) -> Result<usize, EncodeError>,
    /// The decoded length a column states, at most [LZ4_DECODED_MAX](crate::lz4::LZ4_DECODED_MAX).
    pub(super) decoded_len: fn(&[u8]) -> Result<usize, PayloadsError>,
    /// Decode a column into a buffer of its decoded length.
    pub(super) decode: fn(&[u8], &mut [u8]) -> Result<(), PayloadsError>,
}

impl PayloadCoder {
    /// Encode `raw` into `out` (its bytes dropped), giving the column's length.
    pub(super) fn encode_into(&self, raw: &[u8], out: &mut Vec<u8>) -> Result<usize, EncodeError> {
        let max = (self.max_len)(raw.len());
        if out.capacity() < max {
            // a fresh buffer comes zeroed from the allocator
            *out = vec![0; max];
        } else {
            out.clear();
            out.resize(max, 0);
        }
        let n = (self.encode)(raw, out)?;
        out.truncate(n);
        Ok(n)
    }

    /// Decode `column` into a new buffer of the length it states, capped.
    pub(super) fn decode(&self, column: &[u8]) -> Result<Vec<u8>, PayloadsError> {
        let len = (self.decoded_len)(column)?;
        let mut out = vec![0u8; len];
        (self.decode)(column, &mut out)?;
        Ok(out)
    }
}

/// The payload codecs past [Codec::RAW] this crate encodes and decodes, in the order [PayloadCodec::Auto] tries them.
pub const CODERS: &[PayloadCoder] = &[LZ4];

/// [Codec::LZ4]: an LZ4 block ([crate::lz4]), decoded once when the chunk is opened.
const LZ4: PayloadCoder = PayloadCoder {
    codec: Codec::LZ4,
    name: "lz4",
    max_len: lz4_max_len,
    encode: |raw, out| lz4_encode(raw, out).map_err(EncodeError::Lz4),
    decoded_len: |column| lz4_decoded_len(column).map_err(PayloadsError::Lz4),
    decode: |column, out| {
        lz4_decode(column, out)
            .map(|_| ())
            .map_err(PayloadsError::Lz4)
    },
};

/// The entry of payload codec `codec` in `coders`; None for [Codec::RAW] and for a codec not there.
pub(super) fn coder_in(coders: &[PayloadCoder], codec: Codec) -> Option<&PayloadCoder> {
    coders.iter().find(|c| c.codec == codec)
}

/// The entry of payload codec `codec` in [CODERS]; None for [Codec::RAW] and for a codec this crate doesn't know.
pub fn coder(codec: Codec) -> Option<&'static PayloadCoder> {
    coder_in(CODERS, codec)
}

/// How a writer picks the codec of a track's payloads columns, chunk by chunk.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum PayloadCodec {
    /// Try every codec of [CODERS] in order and keep the first that is smaller than the bytes as they are, and
    /// smaller than every one before it; else [Codec::RAW].
    #[default]
    Auto,
    /// [Codec::RAW] always: no encoder runs. For payloads that don't compress, such as encrypted traffic.
    Raw,
    /// This codec always, smaller or not: [Codec::RAW] or one of [CODERS]. Another number is refused as
    /// [EncodeError::UnknownCodec].
    Named(Codec),
}

impl PayloadCodec {
    /// [Codec::LZ4] always.
    pub const LZ4: Self = Self::Named(Codec::LZ4);
}
