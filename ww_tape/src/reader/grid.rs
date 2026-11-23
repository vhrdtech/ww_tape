//! Box queries over a grid's tiles: the chunks of a time range and a box of cells, from the index alone; see
//! [grid](crate::grid).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Range;
use shrink_wrap::prelude::DeserializeShrinkWrap;

use super::{ByteSource, R, ReadError, TapeReader};
use crate::index::IndexEntry;
use crate::state::{ItemHash, Slot};
use crate::{BlockKind, Describe, TileBox, TileCoords, TileLayout, TrackFacts, TrackId};

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
    /// order. Each entry's tile number is tested by shift and mask, no chunk read; only an entry without a tile
    /// number (a scan made it) or with one the grid holds no tile of has its chunk's head read
    /// ([Self::read_chunk_head]). A series, of no fixed axes, is tested by its time alone. The entries are
    /// [Self::track_entries], refused as it refuses them; a chunk whose track's definition in force has another
    /// layout than the box's, or doesn't read, is [ReadError::GridLayout].
    pub fn grid_tiles(
        &self,
        i: usize,
        track_id: TrackId,
        time: Range<i64>,
        tiles: &TileBox,
    ) -> R<Vec<GridTile>, S> {
        let mut out = Vec::new();
        if time.is_empty() {
            return Ok(out);
        }
        let meets = |e: &IndexEntry| {
            e.kind == BlockKind::Series && e.first_tick < time.end && time.start <= e.last_tick
        };
        let layout = tiles.layout();
        let mut layouts = BTreeMap::new();
        let mut buf = Vec::new();
        for entry in self.track_entries(i, track_id)?.into_iter().filter(meets) {
            if self.layout_at(i, track_id, &entry, &mut layouts)? != Some(*layout) {
                return Err(ReadError::GridLayout {
                    offset: entry.offset,
                    track_id,
                });
            }
            if layout.axes() == 0 {
                out.push(GridTile {
                    entry,
                    tile: TileCoords::NONE,
                });
                continue;
            }
            if tiles.is_empty() {
                continue;
            }
            let tile = match entry.tile.and_then(|n| layout.unpack(n)) {
                Some(c) => tiles.holds_coords(&c).then_some(c),
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

    /// The tile layout of the definition of `entry`'s track in force at it, None when none reads or it isn't track
    /// `track_id`'s; `seen` keeps the layouts of the definitions already read, by their hash.
    fn layout_at(
        &self,
        i: usize,
        track_id: TrackId,
        entry: &IndexEntry,
        seen: &mut BTreeMap<ItemHash, Option<TileLayout>>,
    ) -> R<Option<TileLayout>, S> {
        let Some(number) = entry.track() else {
            return Ok(None);
        };
        let Some(hash) = self.hash_at(i, Slot::Track { number }, entry.offset)? else {
            return Ok(None);
        };
        if let Some(l) = seen.get(&hash) {
            return Ok(*l);
        }
        let item = self.item_by_hash(i, hash)?.item;
        let l = item.and_then(|s| match Describe::from_ww_bytes(&s.item) {
            Ok(Describe::Track(t)) if t.id() == track_id => Some(TrackFacts::of(&t).tiles),
            _ => None,
        });
        seen.insert(hash, l);
        Ok(l)
    }
}
