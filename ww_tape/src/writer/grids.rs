//! The newest series chunk of each track and tile a writer placed, in sample order, so it marks a chunk exactly where
//! [marks](crate::series::marks) says ([FLAG_GRID_START](crate::FLAG_GRID_START)).
//!
//! The rule runs within a piece, as verify checks it. The newest chunks of the pieces before only decide the mark of
//! a track and tile's first chunk in the piece, which no reader goes on from: a cut marks nothing by itself. A resume
//! takes the newest chunks from the chunks it keeps; a track whose kept chunk doesn't read has none, and its next
//! chunk is marked. Only chunks of level 0 count: an overview chunk has no mark.

use alloc::collections::BTreeMap;

use crate::ChunkHead;
use crate::series::marks::{self, Key, Newest};

/// The newest chunks placed, see the [module docs](self).
#[derive(Clone, Debug, Default)]
pub(crate) struct Grids {
    /// In this piece
    piece: BTreeMap<Key, Newest>,
    /// In the pieces before, for the tracks and tiles this piece holds no chunk of yet
    before: BTreeMap<Key, Newest>,
    /// The key of the last counted chunk placed and its newest in the piece before it: a block taking that chunk's
    /// place gets it back
    undo: Option<(Key, Option<Newest>)>,
}

impl Grids {
    /// Whether the chunk of `head` is due a grid start mark: a series chunk of level 0 out of sample order or whose
    /// grid isn't the one of its track and tile's newest chunk, or whose track and tile have none. With `replacing`,
    /// against the newest before the last counted chunk, whose place it takes.
    pub(crate) fn due(&self, head: &ChunkHead, replacing: bool) -> bool {
        let Some((key, chunk)) = marks::of(head) else {
            return false;
        };
        let newest = match &self.undo {
            Some((k, before)) if replacing && *k == key => before.as_ref(),
            _ => self.piece.get(&key),
        };
        newest
            .or_else(|| self.before.get(&key))
            .is_none_or(|n| n.due(&chunk, head.first_item))
    }

    /// Take the counted block placed, the chunk of `head` or another block (None); with `replaced`, it takes the
    /// place of the last counted block, whose newest is taken back first.
    pub(crate) fn placed(&mut self, head: Option<&ChunkHead>, replaced: bool) {
        let undo = self.undo.take();
        if replaced && let Some((k, before)) = undo {
            match before {
                Some(n) => self.piece.insert(k, n),
                None => self.piece.remove(&k),
            };
        }
        if let Some(head) = head
            && let Some(key) = self.take(head)
        {
            self.undo = Some(key);
        }
    }

    /// Take the chunk of `head` into the piece's newest; its key and the newest before, for a series chunk of level 0.
    fn take(&mut self, head: &ChunkHead) -> Option<(Key, Option<Newest>)> {
        let (key, chunk) = marks::of(head)?;
        let before = self.piece.get(&key).copied();
        let newest = before.map_or(chunk, |n| n.then(chunk, head.first_item));
        self.piece.insert(key, newest);
        Some((key, before))
    }

    /// Take the chunk of `head`, deferred: it is placed before any later block of its track.
    pub(crate) fn deferred(&mut self, head: &ChunkHead) {
        _ = self.take(head);
    }

    /// Take the chunk of `head`, the newest of its track and tile in a piece before, unless these grids hold one of
    /// that track and tile.
    pub(crate) fn seed(&mut self, head: &ChunkHead) {
        if let Some((key, chunk)) = marks::of(head) {
            self.before.entry(key).or_insert(chunk);
        }
    }

    /// Take the newest chunks of `older`, those of the pieces before, for the tracks and tiles these grids hold none
    /// of.
    pub(crate) fn after(&mut self, older: &Self) {
        for (key, n) in older.piece.iter().chain(&older.before) {
            if !self.piece.contains_key(key) {
                self.before.entry(*key).or_insert(*n);
            }
        }
    }

    /// The grids the next piece of a cut starts with: this piece's newest chunks become the pieces' before.
    #[must_use]
    pub(crate) fn next_piece(mut self) -> Self {
        let piece = core::mem::take(&mut self.piece);
        self.before.extend(piece);
        self.undo = None;
        self
    }

    /// Forget the newest chunks of track `track_id`: a chunk of it a resume keeps doesn't read.
    pub(crate) fn unread(&mut self, track_id: crate::TrackId) {
        self.piece.retain(|(t, _), _| *t != track_id);
        self.before.retain(|(t, _), _| *t != track_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::series::SeriesGrid;
    use crate::{BlockKind, EndMode, ItemNum, Magnitude, TileCoords, TrackId, TrackNum};

    /// A chunk of 8 samples from `first` on `grid`.
    fn head(grid: SeriesGrid, level: u8, first: u64) -> ChunkHead {
        ChunkHead {
            kind: BlockKind::Series,
            track_id: TrackId::new(7),
            track_number: TrackNum::try_new(1).unwrap(),
            first_item: ItemNum::new(first),
            count: 8,
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
    /// taking the last one's place is checked against the newest before it.
    #[test]
    fn a_grid_start_is_due_where_the_grid_or_scale_changes() {
        let g = SeriesGrid::start(0, 0, Magnitude::ONE);
        let mut grids = Grids::default();
        assert!(grids.due(&head(g, 0, 0), false));
        assert!(!grids.due(&head(g, 1, 0), false));
        grids.placed(Some(&head(g, 0, 0)), false);
        assert!(!grids.due(&head(g, 0, 8), false));
        let milli = g.rescaled(Magnitude::pow10(-3));
        assert!(grids.due(&head(milli, 0, 8), false));
        assert!(grids.due(&head(SeriesGrid::start(9, 900, Magnitude::ONE), 0, 9), false));
        grids.placed(Some(&head(milli, 0, 8)), false);
        assert!(grids.due(&head(g, 0, 16), false));
        assert!(!grids.due(&head(g, 0, 8), true));
        grids.placed(Some(&head(g, 0, 8)), true);
        assert!(!grids.due(&head(g, 0, 16), false));
        grids.unread(TrackId::new(7));
        assert!(grids.due(&head(g, 0, 16), false));
    }

    /// A chunk out of sample order is due and leaves the newest as it is: written 0, 16, 8, the chunk at 8 is marked
    /// on the grid of the one at 16, and one at 24 goes on from the one at 16.
    #[test]
    fn a_chunk_out_of_sample_order_is_due() {
        let g = SeriesGrid::start(0, 0, Magnitude::ONE);
        let milli = g.rescaled(Magnitude::pow10(-3));
        let mut grids = Grids::default();
        grids.placed(Some(&head(g, 0, 0)), false);
        grids.placed(Some(&head(milli, 0, 16)), false);
        assert!(grids.due(&head(milli, 0, 8), false));
        grids.placed(Some(&head(milli, 0, 8)), false);
        assert!(!grids.due(&head(milli, 0, 24), false));
    }

    /// After a cut, the pieces before decide only a track's first chunk in the piece.
    #[test]
    fn the_pieces_before_decide_the_first_chunk() {
        let g = SeriesGrid::start(0, 0, Magnitude::ONE);
        let mut grids = Grids::default();
        grids.placed(Some(&head(g, 0, 64)), false);
        let mut next = grids.next_piece();
        assert!(!next.due(&head(g, 0, 72), false));
        assert!(next.due(&head(g, 0, 0), false));
        next.placed(Some(&head(g, 0, 0)), false);
        assert!(!next.due(&head(g, 0, 8), false));
    }
}
