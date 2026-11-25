//! Where a series chunk's grid start mark ([FLAG_GRID_START](crate::FLAG_GRID_START)) is due, the one rule the
//! writer marks by and verify checks.
//!
//! The rule runs in file order over the series chunks of level 0 of one track and tile, against the **newest** chunk
//! so far in sample order: the one whose samples end last. A chunk is due a mark when it doesn't start at or after
//! the newest one's end (it is out of sample order), or when its grid or scale isn't the newest one's. A chunk out of
//! sample order leaves the newest one as it is. So an unmarked chunk follows, in samples and in the file, a chunk on
//! its own grid, and a reader that goes on from its predecessor in sample order goes on from the right grid.

use crate::series::SeriesGrid;
use crate::{BlockKind, ChunkHead, ItemNum, TileCoords, TrackId};

/// A series track and one of its tiles: [TileCoords::NONE] for a series.
pub(crate) type Key = (TrackId, TileCoords);

/// The newest series chunk of a track and tile in sample order: its grid and where its samples end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Newest {
    grid: SeriesGrid,
    end: ItemNum,
}

/// The key and the newest of the series chunk of level 0 of `head` taken alone; None for any other chunk.
pub(crate) fn of(head: &ChunkHead) -> Option<(Key, Newest)> {
    let grid = head.grid?;
    (head.kind == BlockKind::Series && head.level == 0).then(|| {
        let end = head.first_item.saturating_add(u64::from(head.count));
        ((head.track_id, head.tile), Newest { grid, end })
    })
}

impl Newest {
    /// Whether `chunk`, of this one's track and tile and taken alone ([of]), is due a mark after this one: it starts
    /// before this one's end, or its grid isn't this one's.
    pub(crate) fn due(&self, chunk: &Self, first: ItemNum) -> bool {
        first < self.end || chunk.grid != self.grid
    }

    /// The newest after `chunk`, starting at `first`: `chunk` when it starts at or after this one's end, else this one.
    #[must_use]
    pub(crate) fn then(self, chunk: Self, first: ItemNum) -> Self {
        if first >= self.end { chunk } else { self }
    }
}
