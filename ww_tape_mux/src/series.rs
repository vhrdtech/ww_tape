//! The series handle: samples of one series track encoded into chunks on the producer's thread.

use std::sync::Arc;

use shrink_wrap::DeserializeShrinkWrap;
use std::time::Duration;

use exact_units::Bytes;
use ww_tape::samples::{Sample, encode, max_encoded_len};
use ww_tape::{
    BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, Describe, EndMode, ItemNum, Ratio64,
    SampleShape, SeriesError, SeriesGrid, Tapes, TrackDef, TrackId, TrackNum,
};
use ww_tape_summary::{BuildError, Scalar, SeriesLeaf, SeriesLeaves, SeriesSummarySpec};

use crate::TapeMux;
use crate::error::{CallError, EncodeError, ItemError, MuxError};
use crate::events::{CHUNK_BYTES_DEFAULT, LATENCY_DEFAULT};
use crate::handle::{Pressure, Sender, Sent};
use crate::open::{Buffers, Encoder, Open, Sealed};
use crate::queue::lock;
use crate::reply::reply;
use crate::shared::Work;
use crate::summaries::ChunkSummary;

/// Default of [SeriesOptions::chunk_samples].
pub const CHUNK_SAMPLES_DEFAULT: u32 = 65_536;

/// How a [SeriesHandle] cuts its samples into chunks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeriesOptions {
    /// Samples of a chunk at most, 1 and up.
    pub chunk_samples: u32,
    /// Bytes of a chunk's samples as they come, at most: the chunk is sealed at the samples that fill it.
    pub chunk_bytes: Bytes<u32>,
    /// How long the first sample of an open chunk waits at most before the writing thread seals the chunk; None:
    /// until the chunk is full or sealed by the producer.
    pub latency: Option<Duration>,
    /// The codec of the sample column.
    pub codec: Codec,
    /// Whether the mux builds the summaries the track declares ([SeriesDef::summaries](ww_tape::SeriesDef)).
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

    fn set_scale(&mut self, scale: exact_units::Magnitude) -> Result<(), BuildError> {
        match self {
            Self::Same(l) => l.set_scale(scale),
            Self::I16(l) => l.set_scale(scale),
            Self::I32(l) => l.set_scale(scale),
            Self::I64(l) => l.set_scale(scale),
        }
    }

    /// Seal the open part and take every leaf.
    fn seal(&mut self) -> Result<Vec<SeriesLeaf>, BuildError> {
        match self {
            Self::Same(l) => l.seal().map(|()| l.take_leaves()),
            Self::I16(l) => l.seal().map(|()| l.take_leaves()),
            Self::I32(l) => l.seal().map(|()| l.take_leaves()),
            Self::I64(l) => l.seal().map(|()| l.take_leaves()),
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

    fn encode(&mut self, buffers: &dyn Buffers) -> Result<Option<Sealed>, EncodeError> {
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
        };
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: self.codec,
            param: 0,
            len: bytes.len(),
        };
        let columns = vec![(spec, bytes)];
        let hash = ww_tape::piece::columns_payload_hash(columns.iter().map(|(_, b)| b.as_slice()));
        let summary = match &mut self.leaves {
            Some((spec, leaves)) => Some(ChunkSummary::Series {
                shape: self.shape,
                period: self.period,
                spec: *spec,
                leaves: leaves.seal().map_err(EncodeError::Summary)?,
            }),
            None => None,
        };
        self.next_item = self.next_item.saturating_add(u64::from(count));
        self.samples.clear();
        Ok(Some(Sealed {
            head,
            columns,
            hash,
            summary,
        }))
    }
}

impl<S: Scalar + Sample + Send> Encoder for SeriesChunk<S> {
    fn seal(&mut self, buffers: &dyn Buffers) -> Result<Option<Sealed>, EncodeError> {
        self.encode(buffers)
    }
}

/// Writes the samples of one series track from one producer, on a grid.
///
/// Samples are encoded into the open chunk on the producer's thread, its payload hashed there too, and the
/// statistics of the summaries' level-0 nodes computed; the writing thread only places the chunk and builds the
/// levels above. A chunk is sealed when it holds [SeriesOptions::chunk_samples] samples or
/// [SeriesOptions::chunk_bytes] bytes, at a new grid ([Self::regrid]), by [Self::seal], and by the writing thread
/// when its first sample waited [SeriesOptions::latency]. Dropping the handle seals and sends the open chunk;
/// [Self::finish] does so and returns the error.
pub struct SeriesHandle<T: Tapes, S: Scalar + Sample + Send + 'static> {
    s: Sender<T>,
    open: Arc<Open<T, SeriesChunk<S>>>,
    track: TrackNum,
}

impl<T: Tapes, S: Scalar + Sample + Send + 'static> SeriesHandle<T, S> {
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
}

impl<T> TapeMux<T>
where
    T: Tapes + Send + 'static,
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
        let (tx, rx) = reply();
        let bytes = self
            .shared
            .control(Work::TrackDef { track, reply: tx }, rx)?
            .ok_or(CallError::Kind { track })?;
        let Ok(Describe::Track(def)) = Describe::from_ww_bytes(&bytes) else {
            return Err(CallError::Kind { track }.into());
        };
        let chunk = series_chunk::<S>(&def, options)?.ok_or(CallError::Kind { track })?;
        let s = Sender::new(self.shared.clone(), Some(track));
        let open = Open::register(&self.shared, s.slot.clone(), chunk, options.latency);
        Ok(SeriesHandle { s, open, track })
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
        leaves,
    }))
}

impl<T: Tapes, S: Scalar + Sample + Send + 'static> SeriesHandle<T, S>
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

    /// Add samples as [Self::push] does, `flags` ORed into the summary nodes they fall in.
    pub fn push_flagged(
        &mut self,
        samples: &[S],
        flags: u32,
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.s.ready()?;
        let mut units = Vec::new();
        let open = {
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
                let room = e
                    .chunk_frames
                    .saturating_sub(e.frames())
                    .saturating_mul(stride);
                let (now, later) = rest.split_at(room.min(rest.len()));
                if let Some((_, l)) = &mut e.leaves {
                    l.push(now, flags)
                        .map_err(|err| CallError::Encode(EncodeError::Summary(err)))?;
                }
                e.samples.extend_from_slice(now);
                rest = later;
                if e.frames() >= e.chunk_frames {
                    units.extend(self.open.seal_locked(&mut e).map_err(CallError::Encode)?);
                }
            }
            e.frames() > 0
        };
        // never held while sending: the writing thread may need the chunk to seal it
        let mut sent = None;
        for (work, bytes) in units {
            sent = Some(self.s.send(work, bytes)?);
        }
        // the latency runs only once the chunks sealed before are queued: the writing thread seals the open chunk
        // behind them, never ahead
        if open {
            self.open.opened();
        }
        Ok(sent)
    }

    /// Put the samples from the next one on grid `g`: a new grid (a gap, a restart) or the same grid at a new scale.
    /// The open chunk is sealed and sent first. Refused when the grid starts after the next sample, a new grid's
    /// first sample isn't more than a period after the last sample's tick, or the scale has no common scale with
    /// the ones before.
    pub fn regrid(&mut self, g: SeriesGrid) -> Result<Option<Sent>, MuxError<T::Error>> {
        let unit = {
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
            let unit = self.open.seal_locked(&mut e).map_err(CallError::Encode)?;
            e.grid = g;
            unit
        };
        match unit {
            Some((work, bytes)) => Ok(Some(self.s.send(work, bytes)?)),
            None => Ok(None),
        }
    }

    /// Seal the open chunk and send it now; None when it holds no sample.
    pub fn seal(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        let unit = {
            let mut e = lock(&self.open.encoder);
            self.open.seal_locked(&mut e).map_err(CallError::Encode)?
        };
        match unit {
            Some((work, bytes)) => Ok(Some(self.s.send(work, bytes)?)),
            None => Ok(None),
        }
    }

    /// Seal and send the open chunk, then return once everything this handle sent is placed.
    pub fn finish(mut self) -> Result<(), MuxError<T::Error>> {
        self.seal()?;
        self.s.flush(false)
    }
}

impl<T: Tapes, S: Scalar + Sample + Send + 'static> Drop for SeriesHandle<T, S> {
    /// Seal and send the open chunk, as far as the mux takes it.
    fn drop(&mut self) {
        let unit = {
            let mut e = lock(&self.open.encoder);
            self.open.seal_locked(&mut e)
        };
        if let Ok(Some((work, bytes))) = unit {
            // a closed or failed mux takes nothing: the samples are lost as after a close
            drop(self.s.send(work, bytes));
        }
    }
}
