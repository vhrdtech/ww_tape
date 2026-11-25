//! The grid of each series track's newest chunk a writer placed, per tile, so it marks a chunk whose grid or scale
//! isn't that one ([FLAG_GRID_START](crate::FLAG_GRID_START)).
//!
//! The grids go on into the next piece of a cut, so a cut marks nothing by itself. A resume takes them from the chunks
//! it keeps; a track whose newest kept chunk doesn't read, or that has no chunk in the piece, has no grid, and its
//! next chunk is marked. Only chunks of level 0 count: an overview chunk has no mark.

use alloc::collections::BTreeMap;

use crate::series::SeriesGrid;
use crate::{BlockKind, ChunkHead, TileCoords, TrackId};

/// A series track and one of its tiles: [TileCoords::NONE] for a series.
type Key = (TrackId, TileCoords);

/// The grids of the newest chunks placed, see the [module docs](self).
#[derive(Clone, Debug, Default)]
pub(crate) struct Grids {
    newest: BTreeMap<Key, SeriesGrid>,
    /// The key of the last counted chunk placed and its grid before it: a block taking that chunk's place gets it back
    undo: Option<(Key, Option<SeriesGrid>)>,
}

/// The key and grid of a series chunk of level 0; None for any other chunk.
fn of(head: &ChunkHead) -> Option<(Key, SeriesGrid)> {
    let grid = head.grid?;
    (head.kind == BlockKind::Series && head.level == 0)
        .then_some(((head.track_id, head.tile), grid))
}

impl Grids {
    /// Whether the chunk of `head` starts a grid or scale: a series chunk of level 0 whose grid isn't the one of its
    /// track and tile's newest chunk placed, or whose track and tile have none. With `replacing`, against the grid
    /// before the last counted chunk, whose place it takes.
    pub(crate) fn due(&self, head: &ChunkHead, replacing: bool) -> bool {
        let Some((key, grid)) = of(head) else {
            return false;
        };
        let newest = match &self.undo {
            Some((k, before)) if replacing && *k == key => before.as_ref(),
            _ => self.newest.get(&key),
        };
        newest != Some(&grid)
    }

    /// Take the counted block placed, the chunk of `head` or another block (None); with `replaced`, it takes the
    /// place of the last counted block, whose grid is taken back first.
    pub(crate) fn placed(&mut self, head: Option<&ChunkHead>, replaced: bool) {
        let undo = self.undo.take();
        if replaced && let Some((k, before)) = undo {
            match before {
                Some(g) => self.newest.insert(k, g),
                None => self.newest.remove(&k),
            };
        }
        if let Some((key, grid)) = head.and_then(of) {
            self.undo = Some((key, self.newest.insert(key, grid)));
        }
    }

    /// Take the grid of the chunk of `head`, deferred: it is placed before any later block of its track.
    pub(crate) fn deferred(&mut self, head: &ChunkHead) {
        if let Some((key, grid)) = of(head) {
            self.newest.insert(key, grid);
        }
    }

    /// Take the grid of the chunk of `head`, the newest of its track and tile in a piece before, unless these grids
    /// hold one of that track and tile.
    pub(crate) fn seed(&mut self, head: &ChunkHead) {
        if let Some((key, grid)) = of(head) {
            self.newest.entry(key).or_insert(grid);
        }
    }

    /// Take the grids of `older`, those of the pieces before, for the tracks and tiles these grids hold none of.
    pub(crate) fn after(&mut self, older: &Self) {
        for (key, grid) in &older.newest {
            self.newest.entry(*key).or_insert(*grid);
        }
    }

    /// Forget the grids of track `track_id`: a chunk of it a resume keeps doesn't read.
    pub(crate) fn unread(&mut self, track_id: TrackId) {
        self.newest.retain(|(t, _), _| *t != track_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EndMode, ItemNum, Magnitude, TrackNum};

    fn head(grid: SeriesGrid, level: u8) -> ChunkHead {
        ChunkHead {
            kind: BlockKind::Series,
            track_id: TrackId::new(7),
            track_number: TrackNum::try_new(1).unwrap(),
            first_item: ItemNum::new(0),
            count: 1,
            first_tick: 0,
            last_tick: 0,
            latest_end: 0,
            time_mode: 0,
            end_mode: EndMode::Absent,
            grid: Some(grid),
            tile: TileCoords::NONE,
            level,
        }
    }

    /// The first chunk, a new grid and a new scale are due; the same grid again, and an overview, are not; a chunk
    /// taking the last one's place is checked against the grid before it.
    #[test]
    fn a_grid_start_is_due_where_the_grid_or_scale_changes() {
        let g = SeriesGrid::start(0, 0, Magnitude::ONE);
        let mut grids = Grids::default();
        assert!(grids.due(&head(g, 0), false));
        assert!(!grids.due(&head(g, 1), false));
        grids.placed(Some(&head(g, 0)), false);
        assert!(!grids.due(&head(g, 0), false));
        let milli = g.rescaled(Magnitude::pow10(-3));
        assert!(grids.due(&head(milli, 0), false));
        assert!(grids.due(&head(SeriesGrid::start(9, 900, Magnitude::ONE), 0), false));
        grids.placed(Some(&head(milli, 0)), false);
        assert!(grids.due(&head(g, 0), false));
        assert!(!grids.due(&head(g, 0), true));
        grids.placed(Some(&head(g, 0)), true);
        assert!(!grids.due(&head(g, 0), false));
        grids.unread(TrackId::new(7));
        assert!(grids.due(&head(g, 0), false));
    }
}
