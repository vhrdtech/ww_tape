//! The grid handle: time steps of one grid track split into its tiles' chunks and encoded on the producer's thread.

use core::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use exact_units::Bytes;
use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::chunk::Chunk;
use ww_tape::flag_runs::{FLAG_RUN_LEN, FLAG_RUNS_MAX, FlagRunsBuilder};
use ww_tape::samples::{Sample, Samples, Scalar as ScalarKind, encode, max_encoded_len};
use ww_tape::series::SCALE_LEN_MAX;
use ww_tape::{
    BlockKind, ChunkHead, Codec, ColumnKind, ColumnSpec, Describe, EndMode, FlagRuns, ItemNum,
    MAX_BLOCK_LEN, PieceStore, Ratio64, SampleError, SampleShape, SeriesError, SeriesGrid,
    TileCoords, TileLayout, TrackDef, TrackId, TrackNum,
};
use ww_tape_summary::{BuildError, Scalar, SeriesLeaf, SeriesLeaves, SeriesSummarySpec};

use crate::TapeMux;
use crate::error::{CallError, EncodeError, ItemError, MuxError};
use crate::events::{CHUNK_BYTES_DEFAULT, LATENCY_DEFAULT};
use crate::handle::{Backlog, Pressure, Sender, Sent};
use crate::open::{Buffers, Completed, Encoder, Open};
use crate::queue::lock;
use crate::summaries::ChunkSummary;

/// Default of [GridOptions::chunk_steps].
pub const CHUNK_STEPS_DEFAULT: u32 = 65_536;

/// How a [GridHandle] cuts its time steps into chunks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GridOptions {
    /// Time steps of a chunk at most, 1 and up; a series' time step is one sample.
    pub chunk_steps: u32,
    /// Bytes of a tile chunk's values as they come, at most: the chunks are completed at the time step that fills
    /// one. One time step of a tile must fit, and fit a block.
    pub chunk_bytes: Bytes<u32>,
    /// How long the first time step of the open chunks waits at most before the writing thread completes them;
    /// None: until they are full or completed by the producer.
    pub latency: Option<Duration>,
    /// The codec of the sample column.
    pub codec: Codec,
    /// Whether the mux builds the summaries the track declares ([GridDef::summaries](ww_tape::GridDef)).
    pub summaries: bool,
    /// The track's first time step: past 0 when a resumed chain goes on with the track.
    pub first_item: ItemNum,
    /// The tick of the first time step: the first grid runs from it at the track's scale.
    pub first_tick: i64,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self {
            chunk_steps: CHUNK_STEPS_DEFAULT,
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
    /// The leaves of a tile of `cells` cells per time step (1 for a series) from time step `first_item` on.
    fn new(
        shape: SampleShape,
        grid: SeriesGrid,
        first_item: ItemNum,
        spec: SeriesSummarySpec,
        cells: u64,
    ) -> Result<Self, BuildError> {
        let scale = grid.scale;
        if shape.element.zero.is_none() {
            return Ok(Self::Same(SeriesLeaves::of_tile(
                shape, scale, first_item, spec, cells,
            )?));
        }
        Ok(match shape.element.summary_element().bits {
            16 => Self::I16(SeriesLeaves::of_tile(
                shape, scale, first_item, spec, cells,
            )?),
            32 => Self::I32(SeriesLeaves::of_tile(
                shape, scale, first_item, spec, cells,
            )?),
            _ => Self::I64(SeriesLeaves::of_tile(
                shape, scale, first_item, spec, cells,
            )?),
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

/// The open chunk of one tile of a grid: its cells of the time steps so far, in tile order.
struct TileChunk<S: Scalar> {
    coords: TileCoords,
    /// Cells of one time step of the tile, fewer at an axis' far edge
    cells: u64,
    samples: Vec<S>,
    leaves: Option<Leaves<S>>,
}

/// Where a handle's box lies in its grid: the cells it covers and the tiles they fall into.
struct BoxPlan {
    /// The box's first cell along each fixed axis
    start: Vec<u32>,
    /// The box's cells along each fixed axis
    cells: Vec<u32>,
    /// The tile extent of each fixed axis
    tile: Vec<u32>,
    /// The box's first tile along each fixed axis
    first_tile: Vec<u32>,
    /// The box's tiles along each fixed axis
    tiles: Vec<u32>,
}

impl BoxPlan {
    /// The plan of the box of cells `cells` in grid `layout`; None unless it is one non-empty range per fixed axis
    /// within the axis' extent, starting and ending at a tile's edge. No range for a series.
    fn new(layout: &TileLayout, cells: &[Range<u32>]) -> Option<Self> {
        if cells.len() != layout.axes() {
            return None;
        }
        let mut plan = Self {
            start: Vec::new(),
            cells: Vec::new(),
            tile: Vec::new(),
            first_tile: Vec::new(),
            tiles: Vec::new(),
        };
        for (r, &(extent, tile)) in cells.iter().zip(layout.extents()) {
            let aligned = |c: u32| c.is_multiple_of(tile) || c == extent;
            if r.start >= r.end || r.end > extent || !aligned(r.start) || !aligned(r.end) {
                return None;
            }
            let first = r.start.checked_div(tile)?;
            plan.start.push(r.start);
            plan.cells.push(r.end.checked_sub(r.start)?);
            plan.tile.push(tile);
            plan.first_tile.push(first);
            plan.tiles.push(r.end.div_ceil(tile).checked_sub(first)?);
        }
        Some(plan)
    }

    /// The tiles of the box in row-major order, the last axis fastest.
    fn tile_coords(&self) -> Option<Vec<TileCoords>> {
        let n = self
            .tiles
            .iter()
            .try_fold(1usize, |n, t| n.checked_mul(usize::try_from(*t).ok()?))?;
        let mut out = Vec::with_capacity(n);
        let mut at = self.first_tile.clone();
        for _ in 0..n {
            out.push(TileCoords::new(&at)?);
            odometer(&mut at, &self.first_tile, &self.tiles);
        }
        Some(out)
    }

    /// Cells of one time step of the box.
    fn box_cells(&self) -> Option<usize> {
        self.cells
            .iter()
            .try_fold(1usize, |n, c| n.checked_mul(usize::try_from(*c).ok()?))
    }

    /// Scalars of a row of the box's `last_cells` cells along its last axis, of a tile's piece of it, and the tiles
    /// along that axis.
    fn row_shape(&self, last_cells: u32, stride: usize) -> Option<(usize, usize, usize)> {
        let row_len = usize::try_from(last_cells).ok()?.checked_mul(stride)?;
        let piece_len = usize::try_from(*self.tile.last()?)
            .ok()?
            .checked_mul(stride)?;
        Some((
            row_len,
            piece_len,
            usize::try_from(*self.tiles.last()?).ok()?,
        ))
    }

    /// The row of tiles that the row of cells `row` (box-relative cells of every axis but the last) falls into:
    /// row-major over the box's tiles of those axes.
    fn tile_row(&self, row: &[u32]) -> Option<usize> {
        let mut base = 0usize;
        for ((r, t), n) in row.iter().zip(&self.tile).zip(&self.tiles) {
            let along = usize::try_from(r.checked_div(*t)?).ok()?;
            base = base
                .checked_mul(usize::try_from(*n).ok()?)?
                .checked_add(along)?;
        }
        Some(base)
    }

    /// Split one time step of the box, `step` (its cells row-major, `stride` scalars each), into the tiles' chunks,
    /// in the order [Self::tile_coords] gives them. Walks the step once, a row of the last axis at a time.
    fn split<S: Scalar>(
        &self,
        step: &[S],
        stride: usize,
        tiles: &mut [TileChunk<S>],
    ) -> Option<()> {
        let Some((&last_cells, rows_cells)) = self.cells.split_last() else {
            // a series: the one tile is the step
            tiles.first_mut()?.samples.extend_from_slice(step);
            return Some(());
        };
        let (row_len, piece_len, last_tiles) = self.row_shape(last_cells, stride)?;
        let zeros = vec![0u32; rows_cells.len()];
        let mut row = zeros.clone();
        let mut offset = 0usize;
        while offset < step.len() {
            let base = self.tile_row(&row)?.checked_mul(last_tiles)?;
            let cells = step.get(offset..offset.checked_add(row_len)?)?;
            for (j, piece) in cells.chunks(piece_len).enumerate() {
                tiles
                    .get_mut(base.checked_add(j)?)?
                    .samples
                    .extend_from_slice(piece);
            }
            offset = offset.checked_add(row_len)?;
            odometer(&mut row, &zeros, rows_cells);
        }
        Some(())
    }
}

/// Step `at` to the next position row-major, the last axis fastest, each axis `a` running from `from[a]` over
/// `len[a]` values; past the end it starts again.
fn odometer(at: &mut [u32], from: &[u32], len: &[u32]) {
    for ((a, f), n) in at.iter_mut().zip(from).zip(len).rev() {
        *a = a.saturating_add(1);
        if *a < f.saturating_add(*n) {
            return;
        }
        *a = *f;
    }
}

/// The open chunks of a grid handle: one per tile of its box, all holding the same time steps.
pub(crate) struct GridChunks<S: Scalar> {
    track: TrackNum,
    track_id: TrackId,
    shape: SampleShape,
    period: Ratio64,
    codec: Codec,
    /// Scalars of a cell: the track's channels' lanes
    stride: usize,
    /// Scalars of one time step of the box
    step_len: usize,
    /// Time steps of a chunk at most
    chunk_steps: usize,
    /// The grid of the open chunks' time steps
    grid: SeriesGrid,
    /// The first time step of the open chunks
    next_item: u64,
    /// Time steps the open chunks hold
    steps: usize,
    /// The flag runs of the open chunks' time steps, the same for every tile
    runs: FlagRunsBuilder,
    /// The summaries the tiles' leaves are built for
    summaries: Option<SeriesSummarySpec>,
    plan: BoxPlan,
    tiles: Vec<TileChunk<S>>,
    /// The grid's layout: its overview levels and the cells of each tile
    layout: TileLayout,
}

impl<S: Scalar + Sample> GridChunks<S> {
    /// The number the next time step gets.
    fn item_num(&self) -> ItemNum {
        let n = u64::try_from(self.steps).unwrap_or(u64::MAX);
        ItemNum::new(self.next_item.saturating_add(n))
    }

    /// Add `steps` whole time steps of the box, flagged `flags`, to every tile's chunk and its leaves.
    fn add_steps(&mut self, steps: &[S], flags: u32) -> Result<(), CallError> {
        let encode = |e| CallError::Encode(EncodeError::Summary(e));
        if let [tile] = self.tiles.as_mut_slice() {
            // one tile: the box is the tile, its steps go in as they come
            if let Some(l) = &mut tile.leaves {
                l.push(steps, flags).map_err(encode)?;
            }
            tile.samples.extend_from_slice(steps);
            return Ok(());
        }
        let from: Vec<usize> = self.tiles.iter().map(|t| t.samples.len()).collect();
        for step in steps.chunks_exact(self.step_len.max(1)) {
            self.plan
                .split(step, self.stride, &mut self.tiles)
                .ok_or(CallError::Encode(EncodeError::Samples(
                    SampleError::BadShape,
                )))?;
        }
        for (tile, from) in self.tiles.iter_mut().zip(from) {
            if let (Some(l), Some(added)) = (&mut tile.leaves, tile.samples.get(from..)) {
                l.push(added, flags).map_err(encode)?;
            }
        }
        Ok(())
    }

    fn encode(
        &self,
        tile: &mut TileChunk<S>,
        buffers: &dyn Buffers,
    ) -> Result<Completed, EncodeError> {
        let cells = tile.samples.len().checked_div(self.stride).unwrap_or(0);
        let mut bytes = buffers.buffer();
        let max = max_encoded_len(self.codec, self.shape, cells).map_err(EncodeError::Samples)?;
        bytes.resize(max, 0);
        let len = encode(self.codec, self.shape, &tile.samples, &mut bytes)
            .map_err(EncodeError::Samples)?;
        bytes.truncate(len);
        let count = u32::try_from(self.steps)
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
            tile: tile.coords,
            level: 0,
        };
        let spec = ColumnSpec {
            kind: ColumnKind::Samples,
            codec: self.codec,
            param: 0,
            len: bytes.len(),
        };
        let mut columns = vec![(spec, bytes)];
        let mut runs = None;
        if !self.runs.is_empty() {
            let mut b = buffers.buffer();
            self.runs.write(&mut b);
            let spec = ColumnSpec {
                kind: ColumnKind::Flags,
                codec: Codec::FLAG_RUNS,
                param: 0,
                len: b.len(),
            };
            columns.push((spec, b));
            runs = columns.last().map(|(_, b)| b.as_slice());
        }
        let of = crate::overview::TileOf {
            head: &head,
            layout: &self.layout,
            shape: self.shape,
            codec: self.codec,
            runs,
        };
        let overviews = of.overviews(&tile.samples, buffers, 1)?;
        let summary = match (&mut tile.leaves, self.summaries) {
            (Some(leaves), Some(spec)) => Some(ChunkSummary::Series {
                shape: self.shape,
                period: self.period,
                spec,
                leaves: leaves.complete().map_err(EncodeError::Summary)?,
                tile: tile.coords,
                tile_cells: tile.cells,
            }),
            _ => None,
        };
        Ok(Completed {
            head,
            columns,
            summary,
            overviews,
        })
    }
}

impl<S: Scalar + Sample + Send> Encoder for GridChunks<S> {
    fn complete(
        &mut self,
        buffers: &dyn Buffers,
        out: &mut dyn FnMut(Completed),
    ) -> Result<(), EncodeError> {
        if self.steps == 0 {
            return Ok(());
        }
        let mut tiles = core::mem::take(&mut self.tiles);
        let done: Result<Vec<Completed>, EncodeError> =
            tiles.iter_mut().map(|t| self.encode(t, buffers)).collect();
        self.tiles = tiles;
        let done = done?;
        let count = u64::try_from(self.steps).unwrap_or(u64::MAX);
        self.next_item = self.next_item.saturating_add(count);
        self.steps = 0;
        self.runs.clear();
        for t in &mut self.tiles {
            t.samples.clear();
        }
        done.into_iter().for_each(out);
        Ok(())
    }
}

/// Writes the time steps of one grid track from one producer: the whole grid, or one box of its tiles.
///
/// A time step of the box is split into the open chunk of each of its tiles, encoded on the producer's thread, its
/// payload hashed there too, and for a series (a grid of no fixed axes) the statistics of the summaries' level-0
/// nodes computed; the writing thread only places the chunks and builds the levels above. The tiles' chunks hold
/// the same time steps and are completed together: when they hold [GridOptions::chunk_steps] steps or a tile's
/// [GridOptions::chunk_bytes] bytes, at a new grid ([Self::regrid]), before steps that would start a flag run past
/// [FLAG_RUNS_MAX](ww_tape::flag_runs::FLAG_RUNS_MAX) ([Self::push_flagged]), by [Self::complete], and by the writing
/// thread when their first step waited [GridOptions::latency]. A tile at an axis' far edge holds the cells left over;
/// a tile no handle's box holds is never written. Dropping the handle completes and sends the open chunks;
/// [Self::finish] does so and returns the error.
///
/// Several handles of one track write disjoint boxes, each from its own thread; two boxes that share a tile write
/// its time steps twice.
pub struct GridHandle<T: PieceStore, S: Scalar + Sample + Send + 'static> {
    s: Sender<T>,
    open: Arc<Open<T, GridChunks<S>>>,
    backlog: Backlog<T>,
    track: TrackNum,
}

impl<T: PieceStore, S: Scalar + Sample + Send + 'static> GridHandle<T, S> {
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
    /// A handle that writes the time steps of grid track `track`, the whole grid, as values of `S`: the scalar of the
    /// track's element. A series' time step is one sample. Refused for a track the mux doesn't hold, an events or
    /// state track, a series of media, values of another scalar, options out of range, or a tile whose time step
    /// doesn't fit a chunk ([CallError::StepTooLarge]).
    pub fn grid<S: Scalar + Sample + Send + 'static>(
        &self,
        track: TrackNum,
        options: GridOptions,
    ) -> Result<GridHandle<T, S>, MuxError<T::Error>> {
        crate::wait::block_on(self.grid_async::<S>(track, options))
    }

    /// [Self::grid] as a future.
    pub async fn grid_async<S: Scalar + Sample + Send + 'static>(
        &self,
        track: TrackNum,
        options: GridOptions,
    ) -> Result<GridHandle<T, S>, MuxError<T::Error>> {
        self.handle(track, None, options).await
    }

    /// A handle that writes the time steps of the box of cells `cells` of grid track `track`: one range of cells per
    /// fixed axis, each starting and ending at a tile's edge ([CallError::TileBox]). Refused as [Self::grid] is.
    pub fn grid_box<S: Scalar + Sample + Send + 'static>(
        &self,
        track: TrackNum,
        cells: &[Range<u32>],
        options: GridOptions,
    ) -> Result<GridHandle<T, S>, MuxError<T::Error>> {
        crate::wait::block_on(self.grid_box_async::<S>(track, cells, options))
    }

    /// [Self::grid_box] as a future.
    pub async fn grid_box_async<S: Scalar + Sample + Send + 'static>(
        &self,
        track: TrackNum,
        cells: &[Range<u32>],
        options: GridOptions,
    ) -> Result<GridHandle<T, S>, MuxError<T::Error>> {
        self.handle(track, Some(cells), options).await
    }

    /// A handle of grid track `track` over the box of cells `cells`, the whole grid for None.
    async fn handle<S: Scalar + Sample + Send + 'static>(
        &self,
        track: TrackNum,
        cells: Option<&[Range<u32>]>,
        options: GridOptions,
    ) -> Result<GridHandle<T, S>, MuxError<T::Error>> {
        let bytes = self.track_def(track).await?;
        let Ok(Describe::Track(def)) = Describe::from_ww_bytes(&bytes) else {
            return Err(CallError::Kind { track }.into());
        };
        let chunks = grid_chunks::<S>(&def, cells, options)?.ok_or(CallError::Kind { track })?;
        if chunks.summaries.is_none() {
            self.plain(track).await?;
        }
        let s = Sender::new(self.shared.clone(), Some(track));
        let open = Open::register(&self.shared, s.slot.clone(), chunks, options.latency);
        Ok(GridHandle {
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
    let cells = series.tile_layout().ok()?.tile_cells(&head.tile)?;
    let column = chunk.find(ColumnKind::Samples, 0)?;
    // the count from the head is bounded by the column's length before anything is decoded
    let frames = u64::from(head.count).checked_mul(cells)?;
    let samples = Samples::of(&column, shape, usize::try_from(frames).ok()?).ok()?;
    let read = Read {
        shape,
        grid,
        first_item: head.first_item,
        cells,
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
        tile: head.tile,
        tile_cells: cells,
    })
}

/// A series chunk's samples read back, for the leaves a handle of their scalar computed.
struct Read<'a, 'b> {
    shape: SampleShape,
    grid: SeriesGrid,
    first_item: ItemNum,
    /// Cells of one time step of the chunk's tile
    cells: u64,
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
        let mut leaves = Leaves::<S>::new(
            self.shape,
            self.grid,
            self.first_item,
            self.spec,
            self.cells,
        )
        .ok()?;
        leaves.push_runs(&values, &self.runs).ok()?;
        leaves.complete().ok()
    }
}

/// The open chunks of grid track `def` over the box of cells `cells` (None: the whole grid), cut as `options` say;
/// None for another kind of track, values of another scalar or options out of range.
fn grid_chunks<S: Scalar + Sample>(
    def: &TrackDef<'_>,
    cells: Option<&[Range<u32>]>,
    options: GridOptions,
) -> Result<Option<GridChunks<S>>, CallError> {
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
    let Ok(layout) = series.tile_layout() else {
        return Ok(None);
    };
    let whole: Vec<Range<u32>> = layout.extents().iter().map(|(e, _)| 0..*e).collect();
    let plan = BoxPlan::new(&layout, cells.unwrap_or(&whole)).ok_or(CallError::TileBox)?;
    let coords = plan.tile_coords().ok_or(CallError::TileBox)?;
    let step_len = plan
        .box_cells()
        .and_then(|c| c.checked_mul(stride))
        .ok_or(CallError::TileBox)?;
    // the largest tile of the box: the edge tiles hold fewer cells
    let tile_cells = coords
        .iter()
        .filter_map(|c| layout.tile_cells(c))
        .max()
        .and_then(|c| usize::try_from(c).ok())
        .ok_or(CallError::TileBox)?;
    // the largest overview tile of the box, at level 1: its three statistics columns may outgrow the tile chunk
    let overview_cells = layout
        .overview(1)
        .filter(|_| layout.overview_levels() > 0)
        .map(|l1| {
            coords
                .iter()
                .filter_map(|c| l1.tile_cells(c))
                .max()
                .unwrap_or(0)
        })
        .and_then(|c| usize::try_from(c).ok())
        .unwrap_or(0);
    let grid = SeriesGrid::start(options.first_item.get(), options.first_tick, series.scale);
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: def.id(),
        track_number: def.number,
        first_item: options.first_item,
        count: 1,
        first_tick: options.first_tick,
        last_tick: options.first_tick,
        latest_end: options.first_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
        tile: coords.first().copied().unwrap_or_default(),
        level: 0,
    };
    let cells = Cells {
        tile: tile_cells,
        overview: overview_cells,
    };
    let Some(chunk_steps) = chunk_steps::<S>(&head, shape, stride, cells, options)? else {
        return Ok(None);
    };
    let summaries = series.summaries.filter(|_| options.summaries);
    let mut tiles = Vec::with_capacity(coords.len());
    for coords in coords {
        let cells = layout.tile_cells(&coords).ok_or(CallError::TileBox)?;
        let leaves = match summaries {
            Some(spec) => Some(
                Leaves::new(shape, grid, options.first_item, spec, cells)
                    .map_err(|e| CallError::Encode(EncodeError::Summary(e)))?,
            ),
            None => None,
        };
        tiles.push(TileChunk {
            coords,
            cells,
            samples: Vec::new(),
            leaves,
        });
    }
    Ok(Some(GridChunks {
        track: def.number,
        track_id: def.id(),
        shape,
        period: series.period,
        codec: options.codec,
        stride,
        step_len,
        chunk_steps,
        grid,
        next_item: options.first_item.get(),
        steps: 0,
        runs: FlagRunsBuilder::new(),
        summaries,
        plan,
        tiles,
        layout,
    }))
}

/// The cells of one time step of a box's largest tile, and of its largest overview tile (level 1; 0 for a grid
/// without overviews).
#[derive(Copy, Clone)]
struct Cells {
    tile: usize,
    overview: usize,
}

/// Time steps of a chunk of tiles of at most `cells` cells of `stride` scalars: at most the options' steps, their
/// bytes as they come at most the options' bytes, the chunk at most a block. The overview chunks of a tile count as
/// chunks: three columns of the level-1 cells each. None for no steps asked; [CallError::StepTooLarge] when one
/// time step fits no chunk. `head` is a chunk head of the track.
fn chunk_steps<S: Scalar + Sample>(
    head: &ChunkHead,
    shape: SampleShape,
    stride: usize,
    cells: Cells,
    options: GridOptions,
) -> Result<Option<usize>, CallError> {
    let Some(asked) = usize::try_from(options.chunk_steps).ok().filter(|n| *n > 0) else {
        return Ok(None);
    };
    let step_bytes = cells
        .tile
        .max(cells.overview.saturating_mul(OVERVIEW_COLUMNS))
        .saturating_mul(stride)
        .saturating_mul(core::mem::size_of::<S>())
        .max(1);
    let chunk_bytes = usize::try_from(options.chunk_bytes.get()).unwrap_or(usize::MAX);
    let too_large = |max: usize| CallError::StepTooLarge {
        bytes: Bytes::new(u64::try_from(step_bytes).unwrap_or(u64::MAX)),
        max: Bytes::new(u64::try_from(max).unwrap_or(u64::MAX)),
    };
    let by_bytes = chunk_bytes.checked_div(step_bytes).unwrap_or(0);
    if by_bytes == 0 {
        return Err(too_large(chunk_bytes));
    }
    let fits = |steps: usize| {
        let fit = |cells: usize, columns| {
            steps
                .checked_mul(cells)
                .is_some_and(|n| chunk_fits_block(head, options.codec, shape, n, columns))
        };
        fit(cells.tile, 1) && (cells.overview == 0 || fit(cells.overview, OVERVIEW_COLUMNS))
    };
    let mut steps = asked.min(by_bytes);
    if !fits(steps) {
        if !fits(1) {
            return Err(too_large(MAX_BLOCK_LEN));
        }
        // the most steps that fit a block: fits(lo), not fits(hi)
        let (mut lo, mut hi) = (1usize, steps);
        while hi.saturating_sub(lo) > 1 {
            let mid = lo.saturating_add(hi.saturating_sub(lo) / 2);
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        steps = lo;
    }
    Ok(Some(steps))
}

/// Columns of an overview chunk's cells: min, max and mean.
const OVERVIEW_COLUMNS: usize = 3;

/// Whether a chunk of `columns` columns of `cells` cells of `shape` coded by `codec` fits a block whatever its values:
/// its head as `head` but for a scale of up to [SCALE_LEN_MAX] bytes, at most [FLAG_RUNS_MAX] flag runs beside.
fn chunk_fits_block(
    head: &ChunkHead,
    codec: Codec,
    shape: SampleShape,
    cells: usize,
    columns: usize,
) -> bool {
    let Ok(samples) = max_encoded_len(codec, shape, cells) else {
        return false;
    };
    let mut specs = vec![
        ColumnSpec {
            kind: ColumnKind::Samples,
            codec,
            param: 0,
            len: samples,
        };
        columns
    ];
    specs.push(ColumnSpec {
        kind: ColumnKind::Flags,
        codec: Codec::FLAG_RUNS,
        param: 0,
        len: FLAG_RUNS_MAX.saturating_mul(FLAG_RUN_LEN),
    });
    let columns = specs;
    // a regrid may lengthen the scale: room for the longest
    ww_tape::chunk::chunk_len(head, &columns, 0)
        .checked_add(SCALE_LEN_MAX)
        .is_some_and(|len| len <= MAX_BLOCK_LEN)
}

impl<T: PieceStore, S: Scalar + Sample + Send + 'static> GridHandle<T, S>
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

    /// The number the next time step gets.
    pub fn next_item(&self) -> ItemNum {
        lock(&self.open.encoder).item_num()
    }

    /// The tiles this handle writes, row-major, the last fixed axis fastest; one, of no coordinates, for a series.
    pub fn tiles(&self) -> Vec<TileCoords> {
        lock(&self.open.encoder)
            .tiles
            .iter()
            .map(|t| t.coords)
            .collect()
    }

    /// Scalars of one time step of the handle's box: its cells times the track's channels' lanes, row-major, the
    /// last fixed axis fastest, each cell's channels interleaved. A series' time step is one sample.
    pub fn step_len(&self) -> usize {
        lock(&self.open.encoder).step_len
    }

    /// Add whole time steps of the handle's box ([Self::step_len] scalars each) on the current grid. Each chunk they
    /// fill is sent: what became of the last.
    pub fn push(&mut self, steps: &[S]) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_flagged(steps, 0)
    }

    /// Add time steps as [Self::push] does, each flagged `flags`: kept in the chunks' flag runs
    /// ([Chunk::flag_runs](ww_tape::Chunk::flag_runs)) and ORed into the summary nodes they fall in. A chunk holds
    /// at most [FLAG_RUNS_MAX](ww_tape::flag_runs::FLAG_RUNS_MAX) runs: time steps that would start one more complete
    /// the chunks first.
    pub fn push_flagged(
        &mut self,
        steps: &[S],
        flags: u32,
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        let open = self.add(steps, flags)?;
        let sent = self.backlog.send(&self.s)?;
        self.opened(open);
        Ok(sent)
    }

    /// [Self::push] as a future, waiting for room by waker.
    ///
    /// Dropped while it waits, the time steps stay added and the chunks they completed stay with the handle: its next
    /// push, regrid, complete, finish or drop sends those chunks first.
    pub async fn push_async(&mut self, steps: &[S]) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_flagged_async(steps, 0).await
    }

    /// [Self::push_flagged] as a future; see [Self::push_async].
    pub async fn push_flagged_async(
        &mut self,
        steps: &[S],
        flags: u32,
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        let open = self.add(steps, flags)?;
        let sent = self.backlog.send_async(&self.s).await?;
        self.opened(open);
        Ok(sent)
    }

    /// Check and add time steps, the chunks they fill into the backlog; whether the open chunks hold steps after.
    fn add(&mut self, steps: &[S], flags: u32) -> Result<bool, MuxError<T::Error>> {
        self.s.ready()?;
        let mut e = lock(&self.open.encoder);
        let step_len = e.step_len;
        if !steps.len().is_multiple_of(step_len) {
            return Err(CallError::Item {
                item: e.item_num(),
                why: ItemError::PartialStep { scalars: step_len },
            }
            .into());
        }
        let backlog = &mut self.backlog;
        let mut rest = steps;
        while !rest.is_empty() {
            // a chunk holds at most FLAG_RUNS_MAX runs: one more completes it first
            if !e.runs.fits(flags) {
                self.open
                    .complete_locked(&mut e, &mut |u| backlog.push(u))
                    .map_err(CallError::Encode)?;
            }
            let room = e.chunk_steps.saturating_sub(e.steps);
            let n = room.min(rest.len().checked_div(step_len).unwrap_or(0));
            let (now, later) = rest.split_at(n.saturating_mul(step_len).min(rest.len()));
            let count = u32::try_from(n)
                .map_err(|_e| CallError::Encode(EncodeError::Samples(SampleError::BadShape)))?;
            e.runs
                .push(count, flags)
                .map_err(|err| CallError::Encode(EncodeError::FlagRuns(err)))?;
            e.add_steps(now, flags)?;
            e.steps = e.steps.saturating_add(n);
            rest = later;
            if e.steps >= e.chunk_steps {
                self.open
                    .complete_locked(&mut e, &mut |u| backlog.push(u))
                    .map_err(CallError::Encode)?;
            }
        }
        // dropped before sending: the writing thread may need the chunks to complete them
        Ok(e.steps > 0)
    }

    /// Start the open chunks' latency once the chunks completed before are queued, so the writing thread completes
    /// them behind those, never ahead.
    fn opened(&self, open: bool) {
        if open {
            self.open.opened();
        }
    }

    /// Put the time steps from the next one on grid `g`: a new grid (a gap, a restart) or the same grid at a new
    /// scale. The open chunks are completed and sent first. Refused when the grid starts after the next time step, a
    /// new grid's first step isn't more than a period after the last step's tick, or the scale has no common scale
    /// with the ones before.
    pub fn regrid(&mut self, g: SeriesGrid) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.set_grid(g)?;
        self.backlog.send(&self.s)
    }

    /// [Self::regrid] as a future; dropped while it waits, the chunks stay with the handle, as for
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
        for t in &mut e.tiles {
            if let Some(l) = &mut t.leaves {
                l.set_scale(g.scale)
                    .map_err(|err| CallError::Encode(EncodeError::Summary(err)))?;
            }
        }
        let backlog = &mut self.backlog;
        self.open
            .complete_locked(&mut e, &mut |u| backlog.push(u))
            .map_err(CallError::Encode)?;
        e.grid = g;
        Ok(())
    }

    /// Complete the open chunks and send them now; None when they hold no time step.
    pub fn complete(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.complete_open()?;
        self.backlog.send(&self.s)
    }

    /// [Self::complete] as a future; dropped while waiting, the chunks stay with the handle, as for
    /// [Self::push_async].
    pub async fn complete_async(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.complete_open()?;
        self.backlog.send_async(&self.s).await
    }

    fn complete_open(&mut self) -> Result<(), MuxError<T::Error>> {
        let mut e = lock(&self.open.encoder);
        let backlog = &mut self.backlog;
        self.open
            .complete_locked(&mut e, &mut |u| backlog.push(u))
            .map_err(CallError::Encode)?;
        Ok(())
    }

    /// Complete and send the open chunks, then return once everything this handle sent is placed.
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

impl<T: PieceStore, S: Scalar + Sample + Send + 'static> Drop for GridHandle<T, S> {
    /// Complete and send the open chunks and any chunk a dropped async call left, as far as the mux takes them; waits
    /// for room on this thread.
    fn drop(&mut self) {
        {
            let mut e = lock(&self.open.encoder);
            let backlog = &mut self.backlog;
            self.open.complete_counted(&mut e, &mut |u| backlog.push(u));
        }
        // a closed or failed mux takes nothing: the time steps are lost as after a close
        drop(self.backlog.send(&self.s));
    }
}
