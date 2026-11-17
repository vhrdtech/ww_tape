//! Series tracks: what a fixed-rate series is ([SeriesDef]), where its samples are in time ([SeriesGrid]) and
//! how its numbers come from wire_weaver's quantity types ([SeriesValue]).
//!
//! A series track ([TrackKind::Series](crate::TrackKind::Series)) holds samples at an exact period, with no time per
//! sample. Its [SeriesDef] says what a sample is: the type of one value as the API declares it (a [TypeRef]: an
//! exact_units `Fixed<Ampere, Milli, i32>`, a `ww_si` quantity, a plain float), the storage [Element] and channels
//! derived from that type ([SampleShape]), the period in ticks of the track's clock per sample (a [Ratio64], exact:
//! 48 kHz on a 1 ns clock is `62500/3`), the scale of the stored numbers at the start, the summaries the writer
//! builds ([SeriesSummarySpec]) and, for samples kept in a media file, the stream they are in ([MediaStream]).
//!
//! # Grids
//!
//! A series track has no records: its samples are in series chunks ([BlockKind::Series]),
//! and each chunk's head carries the [SeriesGrid] its samples are on, so a chunk times and scales its samples alone.
//! A grid names a sample index, its tick and the scale of the stored numbers:
//!
//! - sample `i` is at tick `grid_tick + (i - grid_index) · period`, rounded down; the grid starts at or before the
//!   chunk's first sample, and the head's first and last tick are its first and last sample's on it;
//! - consecutive chunks of one stretch of recording carry the same grid, so no sample time moves by rounding however
//!   the samples are cut into chunks;
//! - a change of scale (a prefix change of an autoranging device) keeps the grid ([SeriesGrid::rescaled]): the
//!   stored numbers of the chunk mean `number · scale` of the sample type's unit; a chunk has one scale, so a writer
//!   seals the open chunk at a change of scale;
//! - a gap or a restart on the same clock starts a new grid ([SeriesGrid::start]) whose first sample is after the
//!   last tick of the chunk before; a device restart that is a new clock is a new track on that clock.
//!
//! A **gap** is a chunk whose grid isn't the one of the chunk before it: its first sample's tick is past the last
//! tick of that chunk by more than a period, and no sample lies between them. A reader draws no line across a gap.
//! Missing sample indexes between two chunks of one grid are lost samples (a chunk damaged or not in this piece):
//! their ticks are known, their values not. [SeriesTimeline] collects the grids of a track from its chunk heads and
//! checks each chunk against its neighbours.
//!
//! The sample chunks are data, like event chunks: each is written once and never repeated, so a series track takes
//! the plain policy ([TrackRepeat::Stream](crate::TrackRepeat::Stream)). The index lists each chunk's first sample
//! and first tick, so a reader finds the chunk of a sample or a time without reading any head.
//!
//! Summary nodes run over sample indexes, so a node may span a change of scale. It is kept at the common scale of its
//! samples, every number of the coarser ones multiplied by a whole factor, and each summary block states its scale
//! ([summary](crate::summary#scales)). The writer gives each grid to
//! [SeriesSummaries::set_grid](crate::summary::SeriesSummaries::set_grid).
//!
//! # Sample codecs and `Series<T>` codecs
//!
//! Two layers code the same numbers for different readers:
//!
//! | | `shrink_wrap` `Series<T>` (SW-60) | ww_tape [sample codecs](crate::samples) (STD-119) |
//! |---|---|---|
//! | where | on the wire, device to host, a batch of about 32 items | in a file, a chunk of up to megabytes |
//! | reads | from the start, item by item, all columns in step | any sample range, a group of 128 (delta) or 1024 (XOR) alone, in place |
//! | codecs | delta, delta of delta, packed, prefix code, XOR; picked per column and batch | raw device formats, delta and delta of delta bit-packed per group, XOR (Gorilla, Chimp) |
//! | costs | MCU-sized: a few hundred instructions per item, a few KB of flash | host-sized: SIMD unpack kernels, about 0.2 ns per value |
//! | scale | a prefix run and a number type per item (`Quantity`) or in the type (`Fixed`); a prefix change restarts the numeric column | one scale per chunk; a prefix change seals the chunk |
//!
//! Neither is a container of the other. A batch is decoded once on the host and its numbers coded again for the
//! file: the wire codec is tuned for a small encoder, the file codec for random access and fast reads.
//!
//! # From a received `Series` batch to chunk columns
//!
//! The numbers stay integers all the way; nothing goes through a float:
//!
//! 1. Decode the batch with `Series::<T>::from_ww_bytes` and walk its items (`iter()`), zero-copy.
//! 2. `T` is a `Fixed<U, P, R>` (or a `Complex`): the stored number is `item.get()`, an `R`, the element is `R`'s and
//!    the scale is `P`'s ([SeriesValue::ELEMENT], [SeriesValue::SCALE]): [SeriesValue::write_parts] puts the
//!    numbers into the open chunk's column, channels interleaved. The scale never changes.
//! 3. `T` is a `ww_si` quantity: its prefix is the grid's scale (`Prefix::magnitude`); while it stays the same,
//!    the number of its `NumericValue` goes into the column as the track's element, widened exactly (an `i16` into
//!    an `i32` column), never narrowed. An item with another prefix seals the chunk; the next chunk carries the same
//!    grid with the new scale ([SeriesGrid::rescaled]). The `Series` itself restarts its numeric column at the same
//!    item, so both layers break at the same place.
//! 4. A sealed column is coded with [samples::encode](crate::samples::encode) in the track's codec and written as a
//!    chunk whose head carries its grid and the ticks [SeriesGrid::chunk_ticks] gives.
//!
//! The batch's time column (`Series<Timed<T>>`) is not stored: the grid gives every sample's tick. A host checks it
//! against the grid and starts a new grid where the device's samples left it (a gap, a restart).

#[cfg(test)]
mod tests;
#[cfg(feature = "alloc")]
mod timeline;
#[cfg(feature = "alloc")]
pub use timeline::{SeriesStretch, SeriesTimeline};

use exact_units::{
    Bounds, Complex, ComplexStorage, Fixed, FixedScale, Magnitude, Ratio64, Rounding, Storage,
    Unlimited,
};
use shrink_wrap::prelude::*;

#[cfg(feature = "std")]
use crate::TypeRefOwned;
use crate::summary::SeriesSummarySpec;
use crate::{BlockKind, ChunkHead, Element, Sample, SampleShape, TypeRef};

/// Most channels of a series: a chunk column and a summary name a channel by a `u16`.
#[expect(
    clippy::as_conversions,
    reason = "u16 to u32 widens; From is not const"
)]
pub const SERIES_CHANNELS_MAX: u32 = u16::MAX as u32;

/// A fixed-rate series: scalar or vector samples of one element type, see the [module docs](self).
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone), validate = def_valid)]
pub struct SeriesDef<'i> {
    /// The type of one value of one channel, unit and scale included: `Fixed<Ampere, Milli, i32>`, a `ww_si`
    /// quantity, `f32`. None: a plain number with no unit.
    pub sample: Option<TypeRef<'i>>,
    /// The element the numbers are stored as, derived from [Self::sample] ([SeriesValue::ELEMENT]), and the
    /// channels of a sample, 1 to [SERIES_CHANNELS_MAX].
    pub shape: SampleShape,
    /// Ticks of the track's clock from one sample to the next, exact.
    pub period: Ratio64,
    /// What a stored number is worth in the sample type's unit at the start: `10⁻³` for `Fixed<Ampere, Milli, i32>`,
    /// 1 for a float. A chunk's [SeriesGrid] may change it.
    pub scale: Magnitude,
    /// The summaries the writer builds; None for none (a wide vector series).
    pub summaries: Option<SeriesSummarySpec>,
    /// The samples are in a media file's stream, decoded on demand, not in chunks.
    pub media: Option<MediaStream<'i>>,
}

/// Where a series' samples are in a media file.
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Eq, Clone))]
pub struct MediaStream<'i> {
    /// Content hash of the media file, as the producer's media reference (a typed describe item) names it.
    pub media: RefVec<'i, u8>,
    /// The stream in the file.
    pub stream: UNib32,
}

/// Why a series definition or chunk doesn't fit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SeriesError {
    /// The definition's shape, period or summaries break a rule: an element this reader doesn't know, no channels or
    /// more than [SERIES_CHANNELS_MAX], a summary spec [SeriesSummaries](crate::summary::SeriesSummaries) refuses,
    /// a state policy.
    Def(&'static str),
    /// A chunk that isn't a series chunk with a grid.
    NotSeriesChunk { first_item: u64 },
    /// A run of no samples, or one numbered past u64.
    NoSamples { first_item: u64 },
    /// The grid starts after the chunk's first sample.
    GridAfterFirst { first_item: u64, grid_index: u64 },
    /// A sample's tick doesn't fit in i64.
    TickOverflow { index: u64 },
    /// A chunk head's first or last tick isn't its samples' on its grid.
    ChunkTicks { first_item: u64 },
    /// A chunk holds samples another chunk holds, on another grid or at another scale.
    Overlap { first_item: u64 },
    /// A new grid whose first sample is at or before the last tick of the chunk before, or whose last is at or
    /// after the first tick of the chunk after.
    TimeNotIncreasing { first_item: u64 },
    /// The grid's scale doesn't encode in [SCALE_LEN_MAX] bytes.
    ScaleTooLong { first_item: u64 },
}

impl core::fmt::Display for SeriesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SeriesError::Def(why) => write!(f, "series definition: {why}"),
            SeriesError::NotSeriesChunk { first_item } => {
                write!(
                    f,
                    "chunk at sample {first_item}: not a series chunk with a grid"
                )
            }
            SeriesError::NoSamples { first_item } => {
                write!(f, "chunk at sample {first_item}: no samples, or past u64")
            }
            SeriesError::GridAfterFirst {
                first_item,
                grid_index,
            } => write!(
                f,
                "chunk at sample {first_item}: grid starts later, at sample {grid_index}"
            ),
            SeriesError::TickOverflow { index } => write!(f, "sample {index}: tick past i64"),
            SeriesError::ChunkTicks { first_item } => write!(
                f,
                "chunk at sample {first_item}: first or last tick not its samples' on its grid"
            ),
            SeriesError::Overlap { first_item } => write!(
                f,
                "chunk at sample {first_item}: holds samples of another chunk on another grid or scale"
            ),
            SeriesError::TimeNotIncreasing { first_item } => write!(
                f,
                "chunk at sample {first_item}: a new grid not after the chunk before or before the chunk after"
            ),
            SeriesError::ScaleTooLong { first_item } => write!(
                f,
                "chunk at sample {first_item}: a scale that doesn't encode in {SCALE_LEN_MAX} bytes"
            ),
        }
    }
}

/// The time grid and scale of a series chunk's samples, carried in its head; see the [module docs](self#grids).
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct SeriesGrid {
    /// The sample the grid counts from, at most the chunk's first sample.
    pub grid_index: u64,
    /// Tick of sample [Self::grid_index].
    pub grid_tick: i64,
    /// What a stored number of the chunk is worth in the sample type's unit.
    pub scale: Magnitude,
}

impl SeriesGrid {
    /// A grid of its own from sample `index` at `tick`: the start of a series, or after a gap or a restart.
    pub const fn start(index: u64, tick: i64, scale: Magnitude) -> Self {
        SeriesGrid {
            grid_index: index,
            grid_tick: tick,
            scale,
        }
    }

    /// The same grid at another scale: after a prefix change.
    pub const fn rescaled(&self, scale: Magnitude) -> Self {
        SeriesGrid {
            grid_index: self.grid_index,
            grid_tick: self.grid_tick,
            scale,
        }
    }

    /// Tick of sample `index` on the grid, rounded down; None before the grid or past i64.
    pub fn tick(&self, period: Ratio64, index: u64) -> Option<i64> {
        let steps = i64::try_from(index.checked_sub(self.grid_index)?).ok()?;
        self.grid_tick
            .checked_add(period.apply_rounded(steps, Rounding::Floor)?)
    }

    /// Whether `other` is the same grid, whatever its scale.
    pub fn same_grid(&self, other: &SeriesGrid) -> bool {
        self.grid_index == other.grid_index && self.grid_tick == other.grid_tick
    }

    /// First and last tick of `count` samples from `first_item` on the grid: what a series chunk head of them holds.
    pub fn chunk_ticks(
        &self,
        period: Ratio64,
        first_item: u64,
        count: u32,
    ) -> Result<(i64, i64), SeriesError> {
        let last_item = u64::from(count)
            .checked_sub(1)
            .and_then(|n| first_item.checked_add(n))
            .ok_or(SeriesError::NoSamples { first_item })?;
        if self.grid_index > first_item {
            return Err(SeriesError::GridAfterFirst {
                first_item,
                grid_index: self.grid_index,
            });
        }
        let first = self
            .tick(period, first_item)
            .ok_or(SeriesError::TickOverflow { index: first_item })?;
        let last = self
            .tick(period, last_item)
            .ok_or(SeriesError::TickOverflow { index: last_item })?;
        Ok((first, last))
    }

    /// The grid of a series chunk's head, checked: its first and last tick are its samples' on the grid, its scale encodes
    /// in [SCALE_LEN_MAX] bytes.
    pub fn of_chunk(head: &ChunkHead, period: Ratio64) -> Result<SeriesGrid, SeriesError> {
        let grid = match (head.kind, head.grid) {
            (BlockKind::Series, Some(g)) => g,
            _ => {
                return Err(SeriesError::NotSeriesChunk {
                    first_item: head.first_item,
                });
            }
        };
        let (first, last) = grid.chunk_ticks(period, head.first_item, head.count)?;
        if head.first_tick != first || head.last_tick != last {
            return Err(SeriesError::ChunkTicks {
                first_item: head.first_item,
            });
        }
        let mut buf = [0u8; SCALE_LEN_MAX];
        if scale_bytes(&grid.scale, &mut buf).is_err() {
            return Err(SeriesError::ScaleTooLong {
                first_item: head.first_item,
            });
        }
        Ok(grid)
    }
}

/// Bytes a scale takes at most: a power of 10 takes 2, one with two more primes about 10.
pub const SCALE_LEN_MAX: usize = 64;

/// The shrink_wrap bytes of `scale` in `buf`.
pub(crate) fn scale_bytes(
    scale: &Magnitude,
    buf: &mut [u8; SCALE_LEN_MAX],
) -> Result<usize, shrink_wrap::Error> {
    scale.to_ww_bytes(buf).map(<[u8]>::len)
}

/// The scale `bytes` hold, refused unless they are exactly the bytes [scale_bytes] writes for it.
pub(crate) fn read_scale(bytes: &[u8]) -> Result<Magnitude, shrink_wrap::Error> {
    let scale = Magnitude::from_ww_bytes(bytes)?;
    let mut again = [0u8; SCALE_LEN_MAX];
    let n = scale_bytes(&scale, &mut again)?;
    if again.get(..n) == Some(bytes) {
        Ok(scale)
    } else {
        Err(shrink_wrap::Error::SubtypeOutOfRange)
    }
}

/// The parts of a [SeriesDef] its check on decode looks at, borrowed or owned.
trait DefParts {
    fn parts(&self) -> (SampleShape, Option<SeriesSummarySpec>);
}

impl DefParts for SeriesDef<'_> {
    fn parts(&self) -> (SampleShape, Option<SeriesSummarySpec>) {
        (self.shape, self.summaries)
    }
}

#[cfg(feature = "std")]
impl DefParts for SeriesDefOwned {
    fn parts(&self) -> (SampleShape, Option<SeriesSummarySpec>) {
        (self.shape, self.summaries)
    }
}

fn def_valid(d: &impl DefParts) -> Result<(), shrink_wrap::Error> {
    let (shape, summaries) = d.parts();
    check_def(shape, summaries).map_err(|_| shrink_wrap::Error::SubtypeOutOfRange)
}

/// The rules of a definition's shape and summaries; the period and scale are valid by their types.
fn check_def(shape: SampleShape, summaries: Option<SeriesSummarySpec>) -> Result<(), SeriesError> {
    if Element::from_code(shape.element.code()).is_none() {
        return Err(SeriesError::Def("an element this reader doesn't know"));
    }
    if shape.channels.0 == 0 || shape.channels.0 > SERIES_CHANNELS_MAX {
        return Err(SeriesError::Def("channels out of range"));
    }
    if let Some(s) = summaries {
        if !(SeriesSummarySpec::BASE_SHIFT_MIN..=SeriesSummarySpec::BASE_SHIFT_MAX)
            .contains(&s.base_shift)
        {
            return Err(SeriesError::Def("summary base shift out of range"));
        }
        if s.stats.is_empty() {
            return Err(SeriesError::Def("summaries with no statistics"));
        }
        if (s.stats.power_mean || s.stats.power_max) && shape.element.parts() != 2 {
            return Err(SeriesError::Def("power of a real element"));
        }
    }
    Ok(())
}

impl<'i> SeriesDef<'i> {
    /// A series of `channels` channels of `V`, whose element and scale `V` gives, one sample per `period` ticks.
    pub fn of<V: SeriesValue>(sample: Option<TypeRef<'i>>, channels: u32, period: Ratio64) -> Self {
        SeriesDef {
            sample,
            shape: SampleShape::new(V::ELEMENT, channels),
            period,
            scale: V::SCALE,
            summaries: None,
            media: None,
        }
    }

    /// Check the definition as a decoded one is checked: a known element, 1 to [SERIES_CHANNELS_MAX] channels, a
    /// summary spec within its ranges.
    pub fn check(&self) -> Result<(), SeriesError> {
        check_def(self.shape, self.summaries)
    }

    /// The grid of the first chunk: sample 0 at `tick`, at the definition's scale.
    pub fn first_grid(&self, tick: i64) -> SeriesGrid {
        SeriesGrid::start(0, tick, self.scale)
    }
}

/// A type whose values a series stores as numbers of an [Element] at a fixed scale: a plain number, an exact_units
/// `Fixed` or `Complex`. It gives a [SeriesDef] its element and scale and puts a value's numbers into a column.
pub trait SeriesValue: Copy {
    /// The scalar a column holds.
    type Scalar: Sample;
    /// The stored element: the scalar's, or a complex one.
    const ELEMENT: Element;
    /// What a stored number is worth in the type's unit.
    const SCALE: Magnitude;

    /// Put the value's numbers (1, or 2 for a complex value: re, im) at the start of `out`; the count put, fewer when
    /// `out` is shorter.
    fn write_parts(self, out: &mut [Self::Scalar]) -> usize;
}

macro_rules! plain_value {
    ($($t:ty),* $(,)?) => {$(
        impl SeriesValue for $t {
            type Scalar = $t;
            const ELEMENT: Element = <$t as Sample>::ELEMENT;
            const SCALE: Magnitude = Magnitude::ONE;
            #[inline(always)]
            fn write_parts(self, out: &mut [$t]) -> usize {
                put(out, [self])
            }
        }
    )*};
}

plain_value!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64, bool);

/// Put `parts` at the start of `out`, as many as fit.
#[inline(always)]
fn put<T: Copy, const N: usize>(out: &mut [T], parts: [T; N]) -> usize {
    for (o, p) in out.iter_mut().zip(parts) {
        *o = p;
    }
    out.len().min(N)
}

impl<U: Copy, P: FixedScale, T: Storage + Sample, B: Bounds<T> + Copy> SeriesValue
    for Fixed<U, P, T, B>
{
    type Scalar = T;
    const ELEMENT: Element = T::ELEMENT;
    const SCALE: Magnitude = P::SCALE;

    #[inline(always)]
    fn write_parts(self, out: &mut [T]) -> usize {
        put(out, [self.get()])
    }
}

macro_rules! complex_value {
    ($($t:ty => $e:ident),* $(,)?) => {$(
        impl<U: Copy, P: FixedScale, B: Unlimited<$t> + Copy> SeriesValue for Complex<U, P, $t, B>
        where
            $t: ComplexStorage,
        {
            type Scalar = $t;
            const ELEMENT: Element = Element::$e;
            const SCALE: Magnitude = P::SCALE;

            #[inline(always)]
            fn write_parts(self, out: &mut [$t]) -> usize {
                let (re, im) = self.get();
                put(out, [re, im])
            }
        }
    )*};
}

complex_value!(i16 => ComplexI16, f32 => ComplexF32);
