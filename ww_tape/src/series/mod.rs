//! Series tracks: what a fixed-rate series is ([GridDef]), where its samples are in time ([SeriesGrid]) and
//! how its numbers come from wire_weaver's quantity types ([SeriesValue]).
//!
//! A series track ([TrackKind::Series](crate::TrackKind::Series)) holds samples at an exact period, with no time per
//! sample. Its items are numbers ([Items::Numbers](crate::Items::Numbers)): the stored element ([ElementDesc]) and
//! the exact_units type of one value, its unit and scale (a [TypeRef](crate::TypeRef): `Fixed<Ampere, Milli, i32>`, a `ww_si`
//! quantity). Its [GridDef] says the rest of what a sample is: the channels of that element ([SampleShape] with
//! the items' element), the period in ticks of the track's clock per sample (a [Ratio64], exact: 48 kHz on a 1 ns
//! clock is `62500/3`), the scale of the stored numbers at the start, the summaries the writer builds
//! ([SeriesSummarySpec]) and, for samples kept in a media file, the stream they are in ([MediaStream]).
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
//!   completes the open chunk at a change of scale;
//! - a gap or a restart on the same clock starts a new grid ([SeriesGrid::start]) whose first sample is more than a
//!   period after the last tick of the chunk before ([is_gap]); a device restart that is a new clock is a new track on that clock.
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
//! `SeriesSummaries::set_grid` (crate `ww_tape_summary`).
//!
//! # Flags
//!
//! A sample may carry a u32 flags word: an overrange, a dropout, a marker. A series chunk keeps the words of its
//! samples in a [ColumnKind::Flags](crate::ColumnKind::Flags) column in [Codec::FLAG_RUNS](crate::Codec::FLAG_RUNS):
//! runs of consecutive samples sharing a word not 0 ([flag_runs](crate::flag_runs)). A chunk with no flagged sample
//! has no such column, so an unflagged series costs nothing.
//!
//! ```text
//! n × run, n from 1 to FLAG_RUNS_MAX, ascending:
//!   u32  first: the run's first sample, counted from the chunk's first
//!   u32  count: samples in the run, 1 and up
//!   u32  flags: their flags word, not 0
//! ```
//!
//! The chunk is the source of the flags: the summaries OR them into their nodes, and a writer resumed after a crash
//! rebuilds those nodes from the kept chunks, their samples and runs (`SeriesSummaries::push_runs`, crate
//! `ww_tape_summary`). [Chunk::flag_runs](crate::Chunk::flag_runs) reads the runs in place.
//!
//! # Sample codecs and `Series<T>` codecs
//!
//! Two layers code the same numbers for different readers:
//!
//! | | `shrink_wrap` `Series<T>` | ww_tape [sample codecs](crate::samples) |
//! |---|---|---|
//! | where | on the wire, device to host, a batch of about 32 items | in a file, a chunk of up to megabytes |
//! | reads | from the start, item by item, all columns in step | any sample range, a group of 128 alone, in place |
//! | codecs | delta, delta of delta, packed, prefix code, XOR; picked per column and batch | raw device formats, delta and delta of delta bit-packed per group, ALP for floats |
//! | costs | MCU-sized: a few hundred instructions per item, a few KB of flash | host-sized: SIMD unpack kernels, about 0.2 ns per value |
//! | scale | a prefix run and a number type per item (`Quantity`) or in the type (`Fixed`); a prefix change restarts the numeric column | one scale per chunk; a prefix change completes the chunk |
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
//!    the scale is `P`'s ([SeriesValue::ELEMENT], [SeriesValue::SCALE]; [Items::numbers](crate::Items::numbers)
//!    builds the track's items from `T`): [SeriesValue::write_parts] puts the
//!    numbers into the open chunk's column, channels interleaved. The scale never changes.
//! 3. `T` is a `ww_si` quantity: its prefix is the grid's scale (`Prefix::magnitude`); while it stays the same,
//!    the number of its `NumericValue` goes into the column as the track's element, widened exactly (an `i16` into
//!    an `i32` column), never narrowed. An item with another prefix completes the chunk; the next chunk carries the
//!    same grid with the new scale ([SeriesGrid::rescaled]). The `Series` itself restarts its numeric column at the
//!    same item, so both layers break at the same place.
//! 4. A completed column is coded with [samples::encode](crate::samples::encode) in the track's codec and written as a
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

use crate::summary::SeriesSummarySpec;
use crate::{BlockKind, ChunkHead, ElementDesc, ItemNum, Sample, SampleShape};

/// Most channels of a series: a chunk column and a summary name a channel by a `u16`.
#[expect(
    clippy::as_conversions,
    reason = "u16 to u32 widens; From is not const"
)]
pub const SERIES_CHANNELS_MAX: u32 = u16::MAX as u32;

/// A fixed-rate series: scalar or vector samples of the track's element, see the [module docs](self).
#[derive_shrink_wrap(owned(feature = "std"), derive(Debug, PartialEq, Clone), validate = def_valid)]
pub struct GridDef<'i> {
    /// Elements of a sample, 1 to [SERIES_CHANNELS_MAX]; the element and the value type are the track's items
    /// ([Items::Numbers](crate::Items::Numbers)).
    pub channels: UNib32,
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
    /// The definition or its track break a rule: no channels or more than [SERIES_CHANNELS_MAX], a summary spec
    /// `SeriesSummaries` (crate `ww_tape_summary`) refuses, items that are no numbers or of an element the raw codec
    /// doesn't code, a state policy.
    Def(&'static str),
    /// A chunk that isn't a series chunk with a grid.
    NotSeriesChunk { first_item: ItemNum },
    /// A run of no samples, or one numbered past u64.
    NoSamples { first_item: ItemNum },
    /// The grid starts after the chunk's first sample.
    GridAfterFirst {
        first_item: ItemNum,
        grid_index: u64,
    },
    /// A sample's tick doesn't fit in i64.
    TickOverflow { index: ItemNum },
    /// A chunk head's first or last tick isn't its samples' on its grid.
    ChunkTicks { first_item: ItemNum },
    /// A chunk holds samples another chunk holds, on another grid or at another scale.
    Overlap { first_item: ItemNum },
    /// A new grid whose first sample isn't more than a period after the last tick of the chunk before, or whose
    /// last isn't more than a period before the first tick of the chunk after ([is_gap]).
    GridTooClose { first_item: ItemNum },
    /// The grid's scale doesn't encode in [SCALE_LEN_MAX] bytes.
    ScaleTooLong { first_item: ItemNum },
}

impl core::fmt::Display for SeriesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Def(why) => write!(f, "series definition: {why}"),
            Self::NotSeriesChunk { first_item } => {
                write!(
                    f,
                    "chunk at sample {first_item}: not a series chunk with a grid"
                )
            }
            Self::NoSamples { first_item } => {
                write!(f, "chunk at sample {first_item}: no samples, or past u64")
            }
            Self::GridAfterFirst {
                first_item,
                grid_index,
            } => write!(
                f,
                "chunk at sample {first_item}: grid starts later, at sample {grid_index}"
            ),
            Self::TickOverflow { index } => write!(f, "sample {index}: tick past i64"),
            Self::ChunkTicks { first_item } => write!(
                f,
                "chunk at sample {first_item}: first or last tick not its samples' on its grid"
            ),
            Self::Overlap { first_item } => write!(
                f,
                "chunk at sample {first_item}: holds samples of another chunk on another grid or scale"
            ),
            Self::GridTooClose { first_item } => write!(
                f,
                "chunk at sample {first_item}: a new grid not a period apart from the chunk before or after"
            ),
            Self::ScaleTooLong { first_item } => write!(
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
        Self {
            grid_index: index,
            grid_tick: tick,
            scale,
        }
    }

    /// The same grid at another scale: after a prefix change.
    pub const fn rescaled(&self, scale: Magnitude) -> Self {
        Self {
            grid_index: self.grid_index,
            grid_tick: self.grid_tick,
            scale,
        }
    }

    /// Tick of sample `index` on the grid, rounded down; None before the grid or past i64.
    pub fn tick(&self, period: Ratio64, index: ItemNum) -> Option<i64> {
        let steps = i64::try_from(index.get().checked_sub(self.grid_index)?).ok()?;
        self.grid_tick
            .checked_add(period.apply_rounded(steps, Rounding::Floor)?)
    }

    /// Whether `other` is the same grid, whatever its scale.
    pub fn same_grid(&self, other: &Self) -> bool {
        self.grid_index == other.grid_index && self.grid_tick == other.grid_tick
    }

    /// First and last tick of `count` samples from `first_item` on the grid: what a series chunk head of them holds.
    pub fn chunk_ticks(
        &self,
        period: Ratio64,
        first_item: ItemNum,
        count: u32,
    ) -> Result<(i64, i64), SeriesError> {
        let last_item = u64::from(count)
            .checked_sub(1)
            .and_then(|n| first_item.checked_add(n))
            .ok_or(SeriesError::NoSamples { first_item })?;
        if self.grid_index > first_item.get() {
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
    pub fn of_chunk(head: &ChunkHead, period: Ratio64) -> Result<Self, SeriesError> {
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

/// Whether a chunk whose first sample is at `first_tick` lies on a new grid after a gap following a chunk whose last
/// sample is at `last_tick`: more than one `period` after it, rounded up.
///
/// A chunk on the same grid at the next sample lies at most that far after, so the ticks of two neighbouring chunks
/// (their index entries) tell a gap from a continuation alone.
pub fn is_gap(period: Ratio64, last_tick: i64, first_tick: i64) -> bool {
    // the next sample on a grid is at most the period rounded up after the last
    let whole = period.apply_rounded(1, Rounding::Ceil).unwrap_or(i64::MAX);
    // two i64 differ by less than 2^64: the difference fits an i128
    i128::from(first_tick).wrapping_sub(i128::from(last_tick)) > i128::from(whole)
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

/// The parts of a [GridDef] its check on decode looks at, borrowed or owned.
trait DefParts {
    fn parts(&self) -> (u32, Option<SeriesSummarySpec>);
}

impl DefParts for GridDef<'_> {
    fn parts(&self) -> (u32, Option<SeriesSummarySpec>) {
        (self.channels.0, self.summaries)
    }
}

#[cfg(feature = "std")]
impl DefParts for GridDefOwned {
    fn parts(&self) -> (u32, Option<SeriesSummarySpec>) {
        (self.channels.0, self.summaries)
    }
}

#[expect(
    clippy::map_err_ignore,
    reason = "shrink_wrap's decode takes its own error type, which holds no cause"
)]
fn def_valid(d: &impl DefParts) -> Result<(), shrink_wrap::Error> {
    let (channels, summaries) = d.parts();
    check_def(channels, summaries).map_err(|_| shrink_wrap::Error::SubtypeOutOfRange)
}

/// The rules of a definition's channels and summaries; the period and scale are valid by their types.
fn check_def(channels: u32, summaries: Option<SeriesSummarySpec>) -> Result<(), SeriesError> {
    if channels == 0 || channels > SERIES_CHANNELS_MAX {
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
    }
    Ok(())
}

impl<'i> GridDef<'i> {
    /// A series of `channels` channels of `V`, whose scale `V` gives, one sample per `period` ticks. The track's
    /// items are [Items::numbers](crate::Items::numbers) of `V`.
    pub fn of<V: SeriesValue>(channels: u32, period: Ratio64) -> Self {
        GridDef {
            channels: UNib32(channels),
            period,
            scale: V::SCALE,
            summaries: None,
            media: None,
        }
    }

    /// Check the definition as a decoded one is checked: 1 to [SERIES_CHANNELS_MAX] channels, a summary spec within
    /// its ranges. What the element asks of the summaries is the track's check ([TrackDef::check_kind](
    /// crate::TrackDef::check_kind)).
    pub fn check(&self) -> Result<(), SeriesError> {
        check_def(self.channels.0, self.summaries)
    }

    /// The shape of a sample: `channels` of `element`, the track's items' element.
    pub fn shape(&self, element: ElementDesc) -> SampleShape {
        SampleShape::new(element, self.channels.0)
    }

    /// The grid of the first chunk: sample 0 at `tick`, at the definition's scale.
    pub fn first_grid(&self, tick: i64) -> SeriesGrid {
        SeriesGrid::start(0, tick, self.scale)
    }
}

/// A type whose values a series stores as numbers of an element at a fixed scale: a plain number, an exact_units
/// `Fixed` or `Complex`.
///
/// It gives a track's items ([Items::numbers](crate::Items::numbers)) their element, a [GridDef] its scale and
/// puts a value's numbers into a column.
pub trait SeriesValue: Copy {
    /// The scalar a column holds.
    type Scalar: Sample;
    /// The stored element: the scalar's, or a complex pair of it.
    const ELEMENT: ElementDesc;
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
            const ELEMENT: ElementDesc = ElementDesc::of_scalar(<$t as Sample>::SCALAR, 1);
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
    const ELEMENT: ElementDesc = ElementDesc::of_scalar(T::SCALAR, 1);
    const SCALE: Magnitude = P::SCALE;

    #[inline(always)]
    fn write_parts(self, out: &mut [T]) -> usize {
        put(out, [self.get()])
    }
}

macro_rules! complex_value {
    ($($t:ty),* $(,)?) => {$(
        impl<U: Copy, P: FixedScale, B: Unlimited<$t> + Copy> SeriesValue for Complex<U, P, $t, B>
        where
            $t: ComplexStorage,
        {
            type Scalar = $t;
            const ELEMENT: ElementDesc = ElementDesc::of_scalar(<$t as Sample>::SCALAR, 2);
            const SCALE: Magnitude = P::SCALE;

            #[inline(always)]
            fn write_parts(self, out: &mut [$t]) -> usize {
                let (re, im) = self.get();
                put(out, [re, im])
            }
        }
    )*};
}

complex_value!(i16, f32);
