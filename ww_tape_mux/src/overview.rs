//! Space overviews of a grid track's tiles, built beside each tile chunk on the producer's thread.
//!
//! A tile chunk's overview chunks follow it in the same batch, level by level ([ww_tape::grid#overviews]); the writing
//! thread writes the chunk and its overviews as one unit ([ww_tape::PieceChain::write_unit]), so a resume after a
//! crash inside it drops the unit whole and the handle sends the chunk again.

use ww_tape::chunk::ChunkHead;
use ww_tape::samples::{Sample, SampleShape, encode, max_encoded_len};
use ww_tape::{Codec, ColumnKind, ColumnSpec, TileCoords, TileLayout};
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

impl TileOf<'_> {
    /// The overview chunks of the tile chunk from its cells `samples`, its time steps one after the other: one per
    /// level of the grid, in level order; none for a grid without levels.
    pub(crate) fn overviews<S: Scalar + Sample>(
        &self,
        samples: &[S],
        buffers: &dyn Buffers,
    ) -> Result<Vec<Completed>, EncodeError> {
        let Self {
            head,
            layout,
            shape,
            codec,
            runs,
        } = *self;
        overview_chunks(head, layout, (shape, codec, runs), samples, buffers)
    }
}

/// [TileOf::overviews] of `head` in `layout`, cells of `shape` coded in `codec` beside the flag runs `runs`.
fn overview_chunks<S: Scalar + Sample>(
    head: &ChunkHead,
    layout: &TileLayout,
    (shape, codec, runs): (SampleShape, Codec, Option<&[u8]>),
    samples: &[S],
    buffers: &dyn Buffers,
) -> Result<Vec<Completed>, EncodeError> {
    let levels = layout.overview_levels();
    let mut out = Vec::new();
    if levels == 0 {
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
