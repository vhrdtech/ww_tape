//! The series handle: samples of one series track encoded into chunks on the producer's thread.

use std::sync::Arc;

use shrink_wrap::DeserializeShrinkWrap;
use std::time::Duration;

use exact_units::Bytes;
use ww_tape::chunk::Chunk;
use ww_tape::flag_runs::FlagRunsBuilder;
use ww_tape::samples::{Sample, Samples, Scalar as ScalarKind, encode, max_encoded_len};
use ww_tape::{
    BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, Describe, EndMode, FlagRuns, ItemNum,
    PieceStore, Ratio64, SampleError, SampleShape, SeriesError, SeriesGrid, TrackDef, TrackId,
    TrackNum,
};
use ww_tape_summary::{BuildError, Scalar, SeriesLeaf, SeriesLeaves, SeriesSummarySpec};

use crate::TapeMux;
use crate::error::{CallError, EncodeError, ItemError, MuxError};
use crate::events::{CHUNK_BYTES_DEFAULT, LATENCY_DEFAULT};
use crate::handle::{Backlog, Pressure, Sender, Sent};
use crate::open::{Buffers, Completed, Encoder, Open};
use crate::queue::lock;
use crate::summaries::ChunkSummary;

/// Default of [SeriesOptions::chunk_samples].
pub const CHUNK_SAMPLES_DEFAULT: u32 = 65_536;

/// How a [SeriesHandle] cuts its samples into chunks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeriesOptions {
    /// Samples of a chunk at most, 1 and up.
    pub chunk_samples: u32,
    /// Bytes of a chunk's samples as they come, at most: the chunk is completed at the samples that fill it.
    pub chunk_bytes: Bytes<u32>,
    /// How long the first sample of an open chunk waits at most before the writing thread completes the chunk; None:
    /// until the chunk is full or completed by the producer.
    pub latency: Option<Duration>,
    /// The codec of the sample column.
    pub codec: Codec,
    /// Whether the mux builds the summaries the track declares ([GridDef::summaries](ww_tape::GridDef)).
    pub summaries: bool,
    /// The track's first sample: past 0 when a resumed chain goes on with the track.
    pub first_item: ItemNum,
    /// The tick of the first sample: the first grid runs from it at the track's scale.
    pub first_tick: i64,
}

impl Default for SeriesOptions {
    fn default() -> Self {
        Self {
            chunk_samples: CHUNK_SAMPLES_DEFAULT,
            chunk_bytes: CHUNK_BYTES_DEFAULT,
            latency: Some(LATENCY_DEFAULT),
            codec: Codec::RAW,
            summaries: true,
            first_item: ItemNum::new(0),
            first_tick: 0,
        }
    }
}

/// A series' leaves in the scalar its statistics are kept in: the samples' own, or a wider one for an element
/// with a zero.
enum Leaves<S: Scalar> {
    Same(SeriesLeaves<S>),
    I16(SeriesLeaves<i16>),
    I32(SeriesLeaves<i32>),
    I64(SeriesLeaves<i64>),
}

impl<S: Scalar + Sample> Leaves<S> {
    fn new(
        shape: SampleShape,
        grid: SeriesGrid,
        first_item: ItemNum,
        spec: SeriesSummarySpec,
    ) -> Result<Self, BuildError> {
        let scale = grid.scale;
        if shape.element.zero.is_none() {
            return Ok(Self::Same(SeriesLeaves::new(
                shape, scale, first_item, spec,
            )?));
        }
        Ok(match shape.element.summary_element().bits {
            16 => Self::I16(SeriesLeaves::new(shape, scale, first_item, spec)?),
            32 => Self::I32(SeriesLeaves::new(shape, scale, first_item, spec)?),
            _ => Self::I64(SeriesLeaves::new(shape, scale, first_item, spec)?),
        })
    }

    fn push(&mut self, samples: &[S], flags: u32) -> Result<(), BuildError> {
        match self {
            Self::Same(l) => l.push(samples, flags),
            Self::I16(l) => l.push_from(samples, flags),
            Self::I32(l) => l.push_from(samples, flags),
            Self::I64(l) => l.push_from(samples, flags),
        }
    }

    /// Push a chunk's samples flagged by its runs, as the handle pushed them.
    fn push_runs(&mut self, samples: &[S], runs: &FlagRuns<'_>) -> Result<(), BuildError> {
        match self {
            Self::Same(l) => l.push_runs(samples, runs),
            Self::I16(l) => l.push_from_runs(samples, runs),
            Self::I32(l) => l.push_from_runs(samples, runs),
            Self::I64(l) => l.push_from_runs(samples, runs),
        }
    }

    fn set_scale(&mut self, scale: exact_units::Magnitude) -> Result<(), BuildError> {
        match self {
            Self::Same(l) => l.set_scale(scale),
            Self::I16(l) => l.set_scale(scale),
            Self::I32(l) => l.set_scale(scale),
            Self::I64(l) => l.set_scale(scale),
        }
    }

    /// Complete the open part and take every leaf.
    fn complete(&mut self) -> Result<Vec<SeriesLeaf>, BuildError> {
        match self {
            Self::Same(l) => l.complete().map(|()| l.take_leaves()),
            Self::I16(l) => l.complete().map(|()| l.take_leaves()),
            Self::I32(l) => l.complete().map(|()| l.take_leaves()),
            Self::I64(l) => l.complete().map(|()| l.take_leaves()),
        }
    }
}

/// The open chunk of a series track.
pub(crate) struct SeriesChunk<S: Scalar> {
    track: TrackNum,
    track_id: TrackId,
    shape: SampleShape,
    period: Ratio64,
    codec: Codec,
    /// Scalars per sample
    stride: usize,
    chunk_frames: usize,
    /// The grid of the open chunk's samples
    grid: SeriesGrid,
    /// The first sample of the open chunk
    next_item: u64,
    samples: Vec<S>,
    /// The flag runs of the open chunk's samples
    runs: FlagRunsBuilder,
    leaves: Option<(SeriesSummarySpec, Leaves<S>)>,
}

impl<S: Scalar + Sample> SeriesChunk<S> {
    fn frames(&self) -> usize {
        self.samples.len().checked_div(self.stride).unwrap_or(0)
    }

    /// The number the next sample gets.
    fn item_num(&self) -> ItemNum {
        let n = u64::try_from(self.frames()).unwrap_or(u64::MAX);
        ItemNum::new(self.next_item.saturating_add(n))
    }

    fn encode(&mut self, buffers: &dyn Buffers) -> Result<Option<Completed>, EncodeError> {
        let frames = self.frames();
        if frames == 0 {
            return Ok(None);
        }
        let mut bytes = buffers.buffer();
        let max = max_encoded_len(self.codec, self.shape, frames).map_err(EncodeError::Samples)?;
        bytes.resize(max, 0);
        let len = encode(self.codec, self.shape, &self.samples, &mut bytes)
            .map_err(EncodeError::Samples)?;
        bytes.truncate(len);
        let count = u32::try_from(frames)
            .map_err(|_e| EncodeError::Samples(ww_tape::SampleError::BadShape))?;
        let first_item = ItemNum::new(self.next_item);
        let (first_tick, last_tick) = self
            .grid
            .chunk_ticks(self.period, first_item, count)
            .map_err(|_e| EncodeError::Summary(BuildError::Invalid("sample ticks past i64")))?;
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: self.track_id,
            track_number: self.track,
            first_item,
            count,
            first_tick,
            last_tick,
            latest_end: last_tick,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(self.grid),
            tile: ww_tape::TileCoords::NONE,
        };
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: self.codec,
            param: 0,
            len: bytes.len(),
        };
        let mut columns = vec![(spec, bytes)];
        if !self.runs.is_empty() {
            let mut runs = buffers.buffer();
            self.runs.write(&mut runs);
            let spec = ColumnSpec {
                kind: ColumnKind::Flags,
                codec: Codec::FLAG_RUNS,
                param: 0,
                len: runs.len(),
            };
            columns.push((spec, runs));
        }
        let summary = match &mut self.leaves {
            Some((spec, leaves)) => Some(ChunkSummary::Series {
                shape: self.shape,
                period: self.period,
                spec: *spec,
                leaves: leaves.complete().map_err(EncodeError::Summary)?,
            }),
            None => None,
        };
        self.next_item = self.next_item.saturating_add(u64::from(count));
        self.samples.clear();
        self.runs.clear();
        Ok(Some(Completed {
            head,
            columns,
            summary,
        }))
    }
}

impl<S: Scalar + Sample + Send> Encoder for SeriesChunk<S> {
    fn complete(&mut self, buffers: &dyn Buffers) -> Result<Option<Completed>, EncodeError> {
        self.encode(buffers)
    }
}

/// Writes the samples of one series track from one producer, on a grid.
///
/// Samples are encoded into the open chunk on the producer's thread, its payload hashed there too, and the
/// statistics of the summaries' level-0 nodes computed; the writing thread only places the chunk and builds the
/// levels above. A chunk is completed when it holds [SeriesOptions::chunk_samples] samples or
/// [SeriesOptions::chunk_bytes] bytes, at a new grid ([Self::regrid]), before samples that would start a flag run past
/// [FLAG_RUNS_MAX](ww_tape::flag_runs::FLAG_RUNS_MAX) ([Self::push_flagged]), by [Self::complete], and by the writing
/// thread when its first sample waited [SeriesOptions::latency]. Dropping the handle completes and sends the open
/// chunk; [Self::finish] does so and returns the error.
pub struct SeriesHandle<T: PieceStore, S: Scalar + Sample + Send + 'static> {
    s: Sender<T>,
    open: Arc<Open<T, SeriesChunk<S>>>,
    backlog: Backlog<T>,
    track: TrackNum,
}

impl<T: PieceStore, S: Scalar + Sample + Send + 'static> SeriesHandle<T, S> {
    /// What the handle does when the queue has no room.
    #[must_use]
    pub fn pressure(mut self, pressure: Pressure) -> Self {
        self.s.pressure = pressure;
        self
    }

    /// Return once everything this handle sent before is placed; a refusal of one of them.
    pub fn flush(&self) -> Result<(), MuxError<T::Error>> {
        self.s.flush(false)
    }

    /// Return once everything this handle sent before is durable; a refusal of one of them.
    pub fn sync(&self) -> Result<(), MuxError<T::Error>> {
        self.s.flush(true)
    }

    /// [Self::flush] as a future, woken by the writing thread. Dropped before, the flush is done all the same.
    pub async fn flush_async(&self) -> Result<(), MuxError<T::Error>> {
        self.s.flush_async(false).await
    }

    /// [Self::sync] as a future; see [Self::flush_async].
    pub async fn sync_async(&self) -> Result<(), MuxError<T::Error>> {
        self.s.flush_async(true).await
    }
}

impl<T> TapeMux<T>
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    /// A handle that writes the samples of series track `track`, as values of `S`: the scalar of the track's
    /// element. Refused for a track the mux doesn't hold, an events or state track, a series of media, samples of
    /// another scalar, or options out of range.
    pub fn series<S: Scalar + Sample + Send + 'static>(
        &self,
        track: TrackNum,
        options: SeriesOptions,
    ) -> Result<SeriesHandle<T, S>, MuxError<T::Error>> {
        crate::wait::block_on(self.series_async::<S>(track, options))
    }

    /// [Self::series] as a future.
    pub async fn series_async<S: Scalar + Sample + Send + 'static>(
        &self,
        track: TrackNum,
        options: SeriesOptions,
    ) -> Result<SeriesHandle<T, S>, MuxError<T::Error>> {
        let bytes = self.track_def(track).await?;
        let Ok(Describe::Track(def)) = Describe::from_ww_bytes(&bytes) else {
            return Err(CallError::Kind { track }.into());
        };
        let chunk = series_chunk::<S>(&def, options)?.ok_or(CallError::Kind { track })?;
        if !options.summaries {
            self.plain(track).await?;
        }
        let s = Sender::new(self.shared.clone(), Some(track));
        let open = Open::register(&self.shared, s.slot.clone(), chunk, options.latency);
        Ok(SeriesHandle {
            s,
            open,
            backlog: Backlog::new(),
            track,
        })
    }
}

/// The summary of series chunk `chunk` of track `def` read back: the leaves its handle computed of its samples,
/// flagged as its flag runs say. None for a chunk of a track without summaries, or one whose samples or flag runs
/// don't read.
pub(crate) fn read_summary(def: &TrackDef<'_>, chunk: &Chunk<'_>) -> Option<ChunkSummary> {
    let (Some(series), Some(shape)) = (def.series(), def.series_shape()) else {
        return None;
    };
    let spec = series.summaries.filter(|_| series.media.is_none())?;
    let head = chunk.head();
    let grid = head.grid?;
    let column = chunk.find(ColumnKind::Samples, 0)?;
    // the count from the head is bounded by the column's length before anything is decoded
    let samples = Samples::of(&column, shape, usize::try_from(head.count).ok()?).ok()?;
    let read = Read {
        shape,
        grid,
        first_item: head.first_item,
        spec,
        samples: &samples,
        runs: chunk.flag_runs().ok()?,
    };
    let leaves = match shape.element.scalar() {
        ScalarKind::I8 => read.leaves::<i8>(),
        ScalarKind::I16 => read.leaves::<i16>(),
        ScalarKind::I32 => read.leaves::<i32>(),
        ScalarKind::I64 => read.leaves::<i64>(),
        ScalarKind::U8 => read.leaves::<u8>(),
        ScalarKind::U16 => read.leaves::<u16>(),
        ScalarKind::U32 => read.leaves::<u32>(),
        ScalarKind::U64 => read.leaves::<u64>(),
        ScalarKind::F32 => read.leaves::<f32>(),
        ScalarKind::F64 => read.leaves::<f64>(),
        ScalarKind::Bool => None,
    }?;
    Some(ChunkSummary::Series {
        shape,
        period: series.period,
        spec,
        leaves,
    })
}

/// A series chunk's samples read back, for the leaves a handle of their scalar computed.
struct Read<'a, 'b> {
    shape: SampleShape,
    grid: SeriesGrid,
    first_item: ItemNum,
    spec: SeriesSummarySpec,
    samples: &'a Samples<'b>,
    runs: FlagRuns<'b>,
}

impl Read<'_, '_> {
    /// The leaves of the samples decoded as `S`, completed at the chunk's end as the handle completes them.
    fn leaves<S: Scalar + Sample>(&self) -> Option<Vec<SeriesLeaf>> {
        let n = self.samples.count().checked_mul(self.samples.lanes())?;
        let mut values = vec![S::default(); n];
        self.samples.decode(0, &mut values).ok()?;
        let mut leaves =
            Leaves::<S>::new(self.shape, self.grid, self.first_item, self.spec).ok()?;
        leaves.push_runs(&values, &self.runs).ok()?;
        leaves.complete().ok()
    }
}

/// The open chunk of series track `def` cut as `options` say; None for another kind of track, samples of another
/// scalar or options out of range.
fn series_chunk<S: Scalar + Sample>(
    def: &TrackDef<'_>,
    options: SeriesOptions,
) -> Result<Option<SeriesChunk<S>>, CallError> {
    let (Some(series), Some(shape)) = (def.series(), def.series_shape()) else {
        return Ok(None);
    };
    if series.media.is_some() || shape.element.scalar() != <S as Sample>::SCALAR {
        return Ok(None);
    }
    let Some(stride) = shape
        .lanes()
        .and_then(|l| usize::try_from(l).ok())
        .filter(|l| *l > 0)
    else {
        return Ok(None);
    };
    let sample_bytes = stride.saturating_mul(core::mem::size_of::<S>()).max(1);
    let by_bytes = usize::try_from(options.chunk_bytes.get())
        .unwrap_or(usize::MAX)
        .checked_div(sample_bytes)
        .unwrap_or(0);
    let Some(chunk_frames) = usize::try_from(options.chunk_samples)
        .ok()
        .map(|n| n.min(by_bytes))
        .filter(|n| *n > 0)
    else {
        return Ok(None);
    };
    let grid = SeriesGrid::start(options.first_item.get(), options.first_tick, series.scale);
    let leaves = match (options.summaries, series.summaries) {
        (true, Some(spec)) => Some((
            spec,
            Leaves::new(shape, grid, options.first_item, spec)
                .map_err(|e| CallError::Encode(EncodeError::Summary(e)))?,
        )),
        _ => None,
    };
    Ok(Some(SeriesChunk {
        track: def.number,
        track_id: def.id(),
        shape,
        period: series.period,
        codec: options.codec,
        stride,
        chunk_frames,
        grid,
        next_item: options.first_item.get(),
        samples: Vec::new(),
        runs: FlagRunsBuilder::new(),
        leaves,
    }))
}

impl<T: PieceStore, S: Scalar + Sample + Send + 'static> SeriesHandle<T, S>
where
    T: 'static,
    T::Error: Send + Sync,
    T::Key: Send,
    T::Sink: Send,
{
    /// The track this handle writes.
    pub fn track(&self) -> TrackNum {
        self.track
    }

    /// The number the next sample gets.
    pub fn next_item(&self) -> ItemNum {
        lock(&self.open.encoder).item_num()
    }

    /// Add samples on the current grid: whole frames of the track's channels, interleaved. Each chunk they fill is
    /// sent: what became of the last.
    pub fn push(&mut self, samples: &[S]) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_flagged(samples, 0)
    }

    /// Add samples as [Self::push] does, each flagged `flags`: kept in the chunk's flag runs
    /// ([Chunk::flag_runs](ww_tape::Chunk::flag_runs)) and ORed into the summary nodes they fall in. A chunk holds
    /// at most [FLAG_RUNS_MAX](ww_tape::flag_runs::FLAG_RUNS_MAX) runs: samples that would start one more complete it
    /// first.
    pub fn push_flagged(
        &mut self,
        samples: &[S],
        flags: u32,
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        let open = self.add(samples, flags)?;
        let sent = self.backlog.send(&self.s)?;
        self.opened(open);
        Ok(sent)
    }

    /// [Self::push] as a future, waiting for room by waker.
    ///
    /// Dropped while it waits, the samples stay added and the chunks they completed stay with the handle: its next
    /// push, regrid, complete, finish or drop sends those chunks first.
    pub async fn push_async(&mut self, samples: &[S]) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_flagged_async(samples, 0).await
    }

    /// [Self::push_flagged] as a future; see [Self::push_async].
    pub async fn push_flagged_async(
        &mut self,
        samples: &[S],
        flags: u32,
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        let open = self.add(samples, flags)?;
        let sent = self.backlog.send_async(&self.s).await?;
        self.opened(open);
        Ok(sent)
    }

    /// Check and add samples, the chunks they fill into the backlog; whether the open chunk holds samples after.
    fn add(&mut self, samples: &[S], flags: u32) -> Result<bool, MuxError<T::Error>> {
        self.s.ready()?;
        let mut e = lock(&self.open.encoder);
        let stride = e.stride;
        if !samples.len().is_multiple_of(stride) {
            return Err(CallError::Item {
                item: e.item_num(),
                why: ItemError::PartialFrame { stride },
            }
            .into());
        }
        let mut rest = samples;
        while !rest.is_empty() {
            // a chunk holds at most FLAG_RUNS_MAX runs: one more completes it first
            if !e.runs.fits(flags)
                && let Some(unit) = self
                    .open
                    .complete_locked(&mut e)
                    .map_err(CallError::Encode)?
            {
                self.backlog.push(unit);
            }
            let room = e
                .chunk_frames
                .saturating_sub(e.frames())
                .saturating_mul(stride);
            let (now, later) = rest.split_at(room.min(rest.len()));
            let frames = u32::try_from(now.len().checked_div(stride).unwrap_or(0))
                .map_err(|_e| CallError::Encode(EncodeError::Samples(SampleError::BadShape)))?;
            e.runs
                .push(frames, flags)
                .map_err(|err| CallError::Encode(EncodeError::FlagRuns(err)))?;
            if let Some((_, l)) = &mut e.leaves {
                l.push(now, flags)
                    .map_err(|err| CallError::Encode(EncodeError::Summary(err)))?;
            }
            e.samples.extend_from_slice(now);
            rest = later;
            if e.frames() >= e.chunk_frames
                && let Some(unit) = self
                    .open
                    .complete_locked(&mut e)
                    .map_err(CallError::Encode)?
            {
                self.backlog.push(unit);
            }
        }
        // dropped before sending: the writing thread may need the chunk to complete it
        Ok(e.frames() > 0)
    }

    /// Start the open chunk's latency once the chunks completed before are queued, so the writing thread completes it
    /// behind them, never ahead.
    fn opened(&self, open: bool) {
        if open {
            self.open.opened();
        }
    }

    /// Put the samples from the next one on grid `g`: a new grid (a gap, a restart) or the same grid at a new scale.
    /// The open chunk is completed and sent first. Refused when the grid starts after the next sample, a new grid's
    /// first sample isn't more than a period after the last sample's tick, or the scale has no common scale with
    /// the ones before.
    pub fn regrid(&mut self, g: SeriesGrid) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.set_grid(g)?;
        self.backlog.send(&self.s)
    }

    /// [Self::regrid] as a future; dropped while it waits, the chunk stays with the handle, as for
    /// [Self::push_async].
    pub async fn regrid_async(
        &mut self,
        g: SeriesGrid,
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.set_grid(g)?;
        self.backlog.send_async(&self.s).await
    }

    fn set_grid(&mut self, g: SeriesGrid) -> Result<(), MuxError<T::Error>> {
        let mut e = lock(&self.open.encoder);
        let next = e.item_num();
        if g.grid_index > next.get() {
            return Err(CallError::Grid(SeriesError::GridAfterFirst {
                first_item: next,
                grid_index: g.grid_index,
            })
            .into());
        }
        if !g.same_grid(&e.grid)
            && let Some(last) = next
                .get()
                .checked_sub(1)
                .and_then(|i| e.grid.tick(e.period, ItemNum::new(i)))
            && let Some(first) = g.tick(e.period, next)
            && !ww_tape::series::is_gap(e.period, last, first)
        {
            return Err(CallError::Grid(SeriesError::GridTooClose { first_item: next }).into());
        }
        if let Some((_, l)) = &mut e.leaves {
            l.set_scale(g.scale)
                .map_err(|err| CallError::Encode(EncodeError::Summary(err)))?;
        }
        if let Some(unit) = self
            .open
            .complete_locked(&mut e)
            .map_err(CallError::Encode)?
        {
            self.backlog.push(unit);
        }
        e.grid = g;
        Ok(())
    }

    /// Complete the open chunk and send it now; None when it holds no sample.
    pub fn complete(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.complete_open()?;
        self.backlog.send(&self.s)
    }

    /// [Self::complete] as a future; dropped while waiting, the chunk stays with the handle, as for [Self::push_async].
    pub async fn complete_async(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.complete_open()?;
        self.backlog.send_async(&self.s).await
    }

    fn complete_open(&mut self) -> Result<(), MuxError<T::Error>> {
        let mut e = lock(&self.open.encoder);
        if let Some(unit) = self
            .open
            .complete_locked(&mut e)
            .map_err(CallError::Encode)?
        {
            self.backlog.push(unit);
        }
        Ok(())
    }

    /// Complete and send the open chunk, then return once everything this handle sent is placed.
    pub fn finish(mut self) -> Result<(), MuxError<T::Error>> {
        self.complete()?;
        self.s.flush(false)
    }

    /// [Self::finish] as a future. Async code finishes a handle so: its drop sends what it holds blocking.
    pub async fn finish_async(mut self) -> Result<(), MuxError<T::Error>> {
        self.complete_async().await?;
        self.s.flush_async(false).await
    }
}

impl<T: PieceStore, S: Scalar + Sample + Send + 'static> Drop for SeriesHandle<T, S> {
    /// Complete and send the open chunk and any chunk a dropped async call left, as far as the mux takes them; waits
    /// for room on this thread.
    fn drop(&mut self) {
        let unit = {
            let mut e = lock(&self.open.encoder);
            self.open.complete_locked(&mut e)
        };
        if let Ok(Some(unit)) = unit {
            self.backlog.push(unit);
        }
        // a closed or failed mux takes nothing: the samples are lost as after a close
        drop(self.backlog.send(&self.s));
    }
}
