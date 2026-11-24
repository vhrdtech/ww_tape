//! Space overviews of a grid track's tiles: built beside each tile chunk on the producer's thread, and those a crash
//! lost built again from the chunk a resumed piece kept.
//!
//! A tile chunk's overview chunks follow it in the same batch, level by level ([ww_tape::grid#overviews]); the writing
//! thread cuts no piece between them, so a resumed piece that kept a tile chunk lacks at most some of its last
//! overviews, which [lost_overviews] builds again before anything else is written.

use std::collections::BTreeSet;

use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::chunk::{Chunk, ChunkHead};
use ww_tape::index::IndexEntry;
use ww_tape::samples::{
    Sample, SampleShape, Samples, Scalar as ScalarKind, encode, max_encoded_len,
};
use ww_tape::{
    BlockKind, Codec, ColumnKind, ColumnSpec, Definitions, Describe, ItemNum, Slot, TileCoords,
    TileLayout, TrackNum,
};
use ww_tape_summary::{OverviewCells, Scalar};

use crate::error::EncodeError;
use crate::open::{Buffers, Completed};

/// A tile chunk as written whose overviews are built: its head, its grid's layout, the shape of a cell, the codec of
/// the statistics columns and its flag runs column.
pub(crate) struct TileOf<'a> {
    pub head: &'a ChunkHead,
    pub layout: &'a TileLayout,
    pub shape: SampleShape,
    pub codec: Codec,
    pub runs: Option<&'a [u8]>,
}

/// Reads the block of an entry into a buffer; whether it was there to read.
type ReadBack<'a, E> = dyn FnMut(&IndexEntry, &mut Vec<u8>) -> Result<bool, E> + 'a;

impl TileOf<'_> {
    /// The overview chunks of the tile chunk from its cells `samples`, its time steps one after the other: one per
    /// level from `from` up to the grid's levels, in level order. None for a grid without levels.
    pub(crate) fn overviews<S: Scalar + Sample>(
        &self,
        samples: &[S],
        buffers: &dyn Buffers,
        from: u8,
    ) -> Result<Vec<Completed>, EncodeError> {
        let Self {
            head,
            layout,
            shape,
            codec,
            runs,
        } = *self;
        overview_chunks(head, layout, (shape, codec, runs), samples, buffers, from)
    }
}

/// [TileOf::overviews] of `head` in `layout`, cells of `shape` coded in `codec` beside the flag runs `runs`.
fn overview_chunks<S: Scalar + Sample>(
    head: &ChunkHead,
    layout: &TileLayout,
    (shape, codec, runs): (SampleShape, Codec, Option<&[u8]>),
    samples: &[S],
    buffers: &dyn Buffers,
    from: u8,
) -> Result<Vec<Completed>, EncodeError> {
    let levels = layout.overview_levels();
    let mut out = Vec::new();
    if levels == 0 || from > levels {
        return Ok(out);
    }
    let bad = || EncodeError::Samples(ww_tape::SampleError::BadShape);
    let extents = tile_extents(layout, &head.tile).ok_or_else(bad)?;
    let lanes = shape
        .lanes()
        .and_then(|l| usize::try_from(l).ok())
        .ok_or_else(bad)?;
    let steps = usize::try_from(head.count).map_err(|_e| bad())?;
    let mask = layout.downsample();
    let mut cells = OverviewCells::of_cells(samples, steps, &extents, lanes, mask)
        .map_err(EncodeError::Overview)?;
    for level in 1..=levels {
        if level >= from {
            let mut columns = Vec::with_capacity(4);
            let means = cells.means();
            for (kind, values) in [
                (ColumnKind::OverviewMin, cells.min()),
                (ColumnKind::OverviewMax, cells.max()),
                (ColumnKind::OverviewMean, means.as_slice()),
            ] {
                columns.push(column(kind, codec, shape, values, lanes, buffers)?);
            }
            if let Some(runs) = runs {
                let mut b = buffers.buffer();
                b.extend_from_slice(runs);
                let spec = ColumnSpec {
                    kind: ColumnKind::Flags,
                    codec: Codec::FLAG_RUNS,
                    param: 0,
                    len: b.len(),
                };
                columns.push((spec, b));
            }
            out.push(Completed {
                head: ChunkHead { level, ..*head },
                columns,
                summary: None,
                overviews: Vec::new(),
            });
        }
        if level < levels {
            cells = cells.halve(mask).map_err(EncodeError::Overview)?;
        }
    }
    Ok(out)
}

/// The cells of tile `tile` along each fixed axis of `layout`.
pub(crate) fn tile_extents(layout: &TileLayout, tile: &TileCoords) -> Option<Vec<u32>> {
    tile.as_slice()
        .iter()
        .enumerate()
        .map(|(axis, c)| {
            let r = layout.cell_range(axis, *c)?;
            r.end.checked_sub(r.start)
        })
        .collect()
}

/// One statistics column of `values`, cells of `lanes` scalars, coded in `codec`.
fn column<S: Sample>(
    kind: ColumnKind,
    codec: Codec,
    shape: SampleShape,
    values: &[S],
    lanes: usize,
    buffers: &dyn Buffers,
) -> Result<(ColumnSpec, Vec<u8>), EncodeError> {
    let cells = values.len().checked_div(lanes).unwrap_or(0);
    let mut bytes = buffers.buffer();
    let max = max_encoded_len(codec, shape, cells).map_err(EncodeError::Samples)?;
    bytes.resize(max, 0);
    let len = encode(codec, shape, values, &mut bytes).map_err(EncodeError::Samples)?;
    bytes.truncate(len);
    let spec = ColumnSpec {
        kind,
        codec,
        param: 0,
        len: bytes.len(),
    };
    Ok((spec, bytes))
}

/// Buffers of no reuse, for the overviews a resume builds again.
struct Fresh;

impl Buffers for Fresh {
    fn buffer(&self) -> Vec<u8> {
        Vec::new()
    }
}

/// A chunk's place among a track's overviews: its track, tile, first time step and level.
type Place = (TrackNum, TileCoords, ItemNum, u8);

/// The overview chunks of tile chunks of `entries` (a resumed piece's blocks), read back by `read`, that the piece
/// lacks, in the order the handles made them: each tile chunk of a grid with levels whose overviews of some levels
/// are not among the entries gets those built again from its cells, as its handle built them. A chunk that doesn't
/// read back, or of a track `defs` doesn't hold, gets none.
pub(crate) fn lost_overviews<E>(
    defs: &Definitions,
    entries: &[IndexEntry],
    read: &mut ReadBack<'_, E>,
) -> Result<Vec<Completed>, E> {
    let chunks = || {
        entries
            .iter()
            .filter(|e| e.kind == BlockKind::Series && e.track().is_some())
    };
    let mut kept: BTreeSet<Place> = BTreeSet::new();
    let mut buf = Vec::new();
    // an overview chunk's tile is in its head; a scan leaves its entry without a tile number
    for e in chunks().filter(|e| e.level != 0) {
        if read(e, &mut buf)?
            && let Some(h) = ww_tape::Block::read(&buf)
                .ok()
                .and_then(|b| b.chunk())
                .map(|c| c.head())
        {
            kept.insert((h.track_number, h.tile, h.first_item, h.level));
        }
    }
    let mut out = Vec::new();
    for e in chunks().filter(|e| e.level == 0) {
        if !read(e, &mut buf)? {
            continue;
        }
        let Some(chunk) = ww_tape::Block::read(&buf).ok().and_then(|b| b.chunk()) else {
            continue;
        };
        out.extend(rebuilt(defs, &chunk, &kept).unwrap_or_default());
    }
    Ok(out)
}

/// The overviews of tile chunk `chunk` that `kept` lacks, from the first level missing on; None for a chunk of no
/// overviews or one that doesn't read.
fn rebuilt(
    defs: &Definitions,
    chunk: &Chunk<'_>,
    kept: &BTreeSet<Place>,
) -> Option<Vec<Completed>> {
    let head = chunk.head();
    let bytes = defs.get(&Slot::Track {
        number: head.track_number,
    })?;
    let Ok(Describe::Track(def)) = Describe::from_ww_bytes(bytes) else {
        return None;
    };
    let (Some(series), Some(shape)) = (def.series(), def.series_shape()) else {
        return None;
    };
    if def.id() != head.track_id {
        return None;
    }
    let layout = series.tile_layout().ok()?;
    let levels = layout.overview_levels();
    let from = (1..=levels)
        .find(|k| !kept.contains(&(head.track_number, head.tile, head.first_item, *k)))?;
    let column = chunk.find(ColumnKind::Samples, 0)?;
    let cells = layout.tile_cells(&head.tile)?;
    // the count from the head is bounded by the column's length before anything is decoded
    let frames = u64::from(head.count).checked_mul(cells)?;
    let samples = Samples::of(&column, shape, usize::try_from(frames).ok()?).ok()?;
    let runs = chunk.find(ColumnKind::Flags, 0).map(|c| c.bytes);
    let again = Again {
        tile: TileOf {
            head: &head,
            layout: &layout,
            shape,
            codec: column.codec,
            runs,
        },
        samples: &samples,
        from,
    };
    match shape.element.scalar() {
        ScalarKind::I8 => again.chunks::<i8>(),
        ScalarKind::I16 => again.chunks::<i16>(),
        ScalarKind::I32 => again.chunks::<i32>(),
        ScalarKind::I64 => again.chunks::<i64>(),
        ScalarKind::U8 => again.chunks::<u8>(),
        ScalarKind::U16 => again.chunks::<u16>(),
        ScalarKind::U32 => again.chunks::<u32>(),
        ScalarKind::U64 => again.chunks::<u64>(),
        ScalarKind::F32 => again.chunks::<f32>(),
        ScalarKind::F64 => again.chunks::<f64>(),
        ScalarKind::Bool => None,
    }
}

/// A kept tile chunk whose overviews from `from` on are built again.
struct Again<'a, 'b> {
    tile: TileOf<'a>,
    samples: &'a Samples<'b>,
    from: u8,
}

impl Again<'_, '_> {
    fn chunks<S: Scalar + Sample>(&self) -> Option<Vec<Completed>> {
        let n = self.samples.count().checked_mul(self.samples.lanes())?;
        let mut values = vec![S::default(); n];
        self.samples.decode(0, &mut values).ok()?;
        self.tile.overviews(&values, &Fresh, self.from).ok()
    }
}
