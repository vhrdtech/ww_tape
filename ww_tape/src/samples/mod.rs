//! Sample columns: the values of a series column ([ColumnKind::Samples](crate::ColumnKind::Samples)) in a
//! [SampleShape].
//!
//! A column holds `count` samples of a [SampleShape]: `channels` elements each, interleaved, a complex element as its
//! two parts. Its [Codec] says how those scalar values (the column's *lanes* per sample) are laid out; the shape
//! comes from the track definition, so a codec id names only the byte transform. [Samples] reads a column in place
//! and decodes any range of samples; [encode] writes one.
//!
//! The codecs themselves, their table and their layouts are in [ww_tape_codecs::samples], which knows scalars and
//! lanes only. This module maps a shape to them once per column: an [Element] to its [Scalar], the channels to the
//! lanes. A codec's error comes back whole, with the shape of the column ([SampleError::Column]).

mod element;
#[cfg(test)]
mod tests;

pub use element::{Element, SampleShape};
pub use ww_tape_codecs::samples::{ALP_GROUP, DELTA_GROUP, Sample, Scalar, alp};

use crate::Codec;
use core::num::NonZeroUsize;
use core::ops::Range;
use ww_tape_codecs::samples as codecs;

/// Why a sample column can't be read or written.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleError {
    /// The shape's element has no scalar this build codes: [Element::LEAVES], or a newer writer's element.
    UnknownElement(Element),
    /// The shape has no channels, or more scalars per sample than fit in memory.
    BadShape,
    /// The codec refused or failed on a column of `shape`.
    Column {
        shape: SampleShape,
        error: codecs::SampleError,
    },
}

impl core::fmt::Display for SampleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SampleError::UnknownElement(e) => {
                write!(
                    f,
                    "element code {} is no element this build codes",
                    e.code()
                )
            }
            SampleError::BadShape => write!(f, "a sample shape without channels or too wide"),
            SampleError::Column { shape, error } => {
                write!(
                    f,
                    "a column of {} × {}: {error}",
                    shape.channels.0, shape.element
                )
            }
        }
    }
}

impl core::error::Error for SampleError {}

/// The scalar and the lanes of `shape`, the codecs' terms for it.
fn lanes_of(shape: SampleShape) -> Result<(Scalar, NonZeroUsize), SampleError> {
    let scalar = shape
        .element
        .scalar_type()
        .ok_or(SampleError::UnknownElement(shape.element))?;
    let lanes = shape
        .lanes()
        .and_then(|l| usize::try_from(l).ok())
        .and_then(NonZeroUsize::new)
        .ok_or(SampleError::BadShape)?;
    Ok((scalar, lanes))
}

/// A codec's error on a column of `shape`.
fn column_err(shape: SampleShape) -> impl Fn(codecs::SampleError) -> SampleError {
    move |error| SampleError::Column { shape, error }
}

/// The most bytes [encode] can write for `count` samples of `shape` in `codec`.
pub fn max_encoded_len(
    codec: Codec,
    shape: SampleShape,
    count: usize,
) -> Result<usize, SampleError> {
    let (scalar, lanes) = lanes_of(shape)?;
    codecs::max_encoded_len(codec, scalar, lanes, count).map_err(column_err(shape))
}

/// Code `values` (whole samples of `shape`, lanes interleaved) in `codec` into `out`, which holds at least
/// [max_encoded_len] bytes; returns the bytes written.
pub fn encode<T: Sample>(
    codec: Codec,
    shape: SampleShape,
    values: &[T],
    out: &mut [u8],
) -> Result<usize, SampleError> {
    let (scalar, lanes) = lanes_of(shape)?;
    if scalar != T::SCALAR {
        return Err(column_err(shape)(codecs::SampleError::WrongType {
            column: scalar,
            asked: T::SCALAR,
        }));
    }
    codecs::encode(codec, lanes, values, out).map_err(column_err(shape))
}

/// A sample column read in place: its codec, shape and count, checked against its bytes.
#[derive(Copy, Clone, Debug)]
pub struct Samples<'b> {
    shape: SampleShape,
    column: codecs::Samples<'b>,
}

impl<'b> Samples<'b> {
    /// Take `bytes` as `count` samples of `shape` coded in `codec`: the codec must code the element and the length
    /// must fit (for the grouped codecs, the group table too). No value is decoded yet. `count` may come from the
    /// file: it is bounded by the column's length before anything is read.
    pub fn new(
        codec: Codec,
        shape: SampleShape,
        count: usize,
        bytes: &'b [u8],
    ) -> Result<Self, SampleError> {
        let (scalar, lanes) = lanes_of(shape)?;
        let column =
            codecs::Samples::new(codec, scalar, lanes, count, bytes).map_err(column_err(shape))?;
        Ok(Samples { shape, column })
    }

    /// The column `column` of a chunk as `count` samples of `shape`.
    pub fn of(
        column: &crate::chunk::Column<'b>,
        shape: SampleShape,
        count: usize,
    ) -> Result<Self, SampleError> {
        Self::new(column.codec, shape, count, column.bytes)
    }

    pub fn codec(&self) -> Codec {
        self.column.codec()
    }

    pub fn shape(&self) -> SampleShape {
        self.shape
    }

    /// Samples in the column.
    pub fn count(&self) -> usize {
        self.column.count()
    }

    /// Scalars per sample.
    pub fn lanes(&self) -> usize {
        self.column.lanes()
    }

    /// Decode samples from `first` on into `out`, whole samples of interleaved lanes: `out.len() / lanes` samples.
    pub fn decode<T: Sample>(&self, first: usize, out: &mut [T]) -> Result<(), SampleError> {
        self.column
            .decode(first, out)
            .map_err(column_err(self.shape))
    }

    /// Groups a reader can decode alone: 1 for a raw format, a group per [DELTA_GROUP] or [ALP_GROUP]
    /// samples for the delta and ALP codecs.
    pub fn group_count(&self) -> usize {
        self.column.group_count()
    }

    /// The samples of group `g` and the bytes of the column that code them, None when `g` is not below
    /// [Self::group_count]. A reader whose chunk has bad segments ([Chunk::range_damaged](crate::Chunk::range_damaged),
    /// offset by the column's offset) decodes the groups whose bytes are intact. A raw format's samples each decode
    /// alone: [Self::sample_bytes] narrows its single group.
    pub fn group_bytes(&self, g: usize) -> Option<(Range<usize>, Range<usize>)> {
        self.column.group_bytes(g)
    }

    /// The bytes that code `samples` in a raw format, None for the grouped codecs (see [Self::group_bytes]) and for
    /// a range whose bit offsets overflow.
    pub fn sample_bytes(&self, samples: Range<usize>) -> Option<Range<usize>> {
        self.column.sample_bytes(samples)
    }
}
