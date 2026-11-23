//! Box queries over a grid's tiles: the chunks of a time range and a box of cells, from the index alone; see
//! [grid](crate::grid).

use alloc::vec::Vec;
use core::ops::Range;

use super::{ByteSource, R, TapeReader};
use crate::index::IndexEntry;
use crate::{BlockKind, TileBox, TileCoords, TrackId};

/// A chunk a box query found: its index entry and its tile's coordinates.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GridTile {
    pub entry: IndexEntry,
    /// The tile the chunk holds; none for a series.
    pub tile: TileCoords,
}

impl<S: ByteSource> TapeReader<S> {
    /// The chunks of grid track `track_id` in piece `i` whose samples meet the ticks `time` and whose tile lies in
    /// `tiles` (a box of cells, [TileLayout::tile_box](crate::TileLayout::tile_box) of the track's layout), in file
    /// order. Each entry's tile number is tested by shift and mask, no chunk read; only an entry a scan made, which
    /// holds no tile number, has its chunk's head read ([Self::read_chunk_head]). A series, of no fixed axes, is
    /// tested by its time alone. The entries are [Self::track_entries], refused as it refuses them.
    pub fn grid_tiles(
        &self,
        i: usize,
        track_id: TrackId,
        time: Range<i64>,
        tiles: &TileBox,
    ) -> R<Vec<GridTile>, S> {
        let entries = self.track_entries(i, track_id)?;
        let meets = |e: &IndexEntry| {
            e.kind == BlockKind::Series && e.first_tick < time.end && time.start <= e.last_tick
        };
        let layout = tiles.layout();
        if layout.axes() == 0 {
            return Ok(entries
                .into_iter()
                .filter(meets)
                .map(|entry| GridTile {
                    entry,
                    tile: TileCoords::NONE,
                })
                .collect());
        }
        let mut out = Vec::new();
        if tiles.is_empty() {
            return Ok(out);
        }
        let mut buf = Vec::new();
        for entry in entries.into_iter().filter(meets) {
            let tile = match entry.tile {
                Some(n) if tiles.holds(n) => layout.unpack(n),
                Some(_) => None,
                None => {
                    let head = self.read_chunk_head(i, &entry, &mut buf)?;
                    tiles.holds_coords(&head.tile).then_some(head.tile)
                }
            };
            if let Some(tile) = tile {
                out.push(GridTile { entry, tile });
            }
        }
        Ok(out)
    }
}
