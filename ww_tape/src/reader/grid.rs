//! Box queries over a grid's tiles: the chunks of a time range and a box of cells, from the index alone; see
//! [grid](crate::grid).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ops::Range;
use shrink_wrap::prelude::DeserializeShrinkWrap;

use super::{ByteSource, R, ReadError, TapeReader};
use crate::index::IndexEntry;
use crate::state::{ItemHash, Slot};
use crate::summary::SummaryHead;
use crate::{
    BlockError, BlockKind, Describe, TileBox, TileCoords, TileLayout, TrackFacts, TrackId,
};

/// What a box query finds: chunks, or summary blocks of a level.
#[derive(Copy, Clone)]
enum Of {
    Chunks,
    Summaries(u8),
}

/// A chunk or summary block a box query found: its index entry and its tile's coordinates.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GridTile {
    pub entry: IndexEntry,
    /// The tile the chunk or summary block holds; none for a series.
    pub tile: TileCoords,
}

impl<S: ByteSource> TapeReader<S> {
    /// The chunks of grid track `track_id` in piece `i` whose samples meet the ticks `time` and whose tile lies in
    /// `tiles` (a box of cells, [TileLayout::tile_box](crate::TileLayout::tile_box) of the track's layout), in file
    /// order. The box's layout names the level: the chunks as written for the track's layout, the overview chunks of
    /// level k for its [TileLayout::overview](crate::TileLayout::overview) k, the box then in cells of that level
    /// ([grid](crate::grid#overviews)). Each entry's tile number is tested by shift and mask, no chunk read; only an entry without a tile
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
        self.tiles_of(i, track_id, time, tiles, Of::Chunks)
    }

    /// The summary blocks of `level` of grid track `track_id` in piece `i` whose items meet the ticks `time` and
    /// whose tile lies in `tiles`, in file order: the blocks of each tile's time pyramid ([summary](
    /// crate::summary#tiles)) a region's timeline merges. Found as [Self::grid_tiles] finds chunks: by the entries'
    /// tile numbers, reading the head only of a block whose entry has none; a series' blocks by their time alone.
    ///
    /// A found block's [GridTile::tile] comes from its index entry, unread: a caller reading the block checks it
    /// against the block's head ([SummaryHead::tile](crate::summary::SummaryHead::tile)) before trusting either, as
    /// `ww_tape_summary::region_timeline` does.
    pub fn grid_summaries(
        &self,
        i: usize,
        track_id: TrackId,
        time: Range<i64>,
        tiles: &TileBox,
        level: u8,
    ) -> R<Vec<GridTile>, S> {
        self.tiles_of(i, track_id, time, tiles, Of::Summaries(level))
    }

    fn tiles_of(
        &self,
        i: usize,
        track_id: TrackId,
        time: Range<i64>,
        tiles: &TileBox,
        of: Of,
    ) -> R<Vec<GridTile>, S> {
        let mut out = Vec::new();
        if time.is_empty() {
            return Ok(out);
        }
        let series_tree = crate::summary::Tree::Series.to_u8();
        let layout = tiles.layout();
        let meets = |e: &IndexEntry| {
            let kind = match of {
                Of::Chunks => e.kind == BlockKind::Series && e.level == layout.level(),
                Of::Summaries(level) => {
                    e.kind == BlockKind::Summary && e.tree == series_tree && e.level == level
                }
            };
            kind && e.first_tick < time.end && time.start <= e.last_tick
        };
        let mut layouts = BTreeMap::new();
        let mut buf = Vec::new();
        for entry in self.track_entries(i, track_id)?.into_iter().filter(meets) {
            // the definition's layout at the box's level; summaries are of the cells as written
            let at = self.layout_at(i, track_id, &entry, &mut layouts)?;
            let at = match of {
                Of::Chunks => at.and_then(|l| l.overview(layout.level())),
                Of::Summaries(_) => at,
            };
            if at != Some(*layout) {
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
                    let tile = match of {
                        Of::Chunks => self.read_chunk_head(i, &entry, &mut buf)?.tile,
                        Of::Summaries(_) => self.read_summary_tile(i, &entry, &mut buf)?,
                    };
                    tiles.holds_coords(&tile).then_some(tile)
                }
            };
            if let Some(tile) = tile {
                out.push(GridTile { entry, tile });
            }
        }
        Ok(out)
    }

    /// The tile of the summary block of `entry` in piece `i`, read whole into `buf`; a block other than a summary
    /// is [ReadError::Block].
    fn read_summary_tile(
        &self,
        i: usize,
        entry: &IndexEntry,
        buf: &mut Vec<u8>,
    ) -> R<TileCoords, S> {
        let block = self.read_block(i, entry, buf)?;
        let (head, _, _) = SummaryHead::read(block.bytes()).map_err(|e| ReadError::Block {
            offset: entry.offset,
            error: BlockError::Summary(e),
        })?;
        Ok(head.tile)
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
