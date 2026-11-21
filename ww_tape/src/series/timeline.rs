//! [SeriesTimeline]: the grids of a series' chunks in order, checked between neighbours, and the tick and scale of
//! any sample.

use alloc::vec::Vec;

use exact_units::{Magnitude, Ratio64};

use super::{SeriesError, SeriesGrid, is_gap};
use crate::ChunkHead;
use crate::ItemNum;

/// Samples `first_item..end_item` of a series on one grid at one scale: one chunk, or consecutive chunks merged.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeriesStretch {
    /// The first sample.
    pub first_item: ItemNum,
    /// One past the last sample.
    pub end_item: ItemNum,
    /// The samples' grid and scale.
    pub grid: SeriesGrid,
}

impl SeriesStretch {
    /// Index of the last sample: `end_item` is above `first_item` in a stretch of a timeline.
    fn last_item(&self) -> ItemNum {
        self.end_item.saturating_sub(1)
    }
}

/// The grids of one series track, from its chunk heads in any order, checked as they come.
///
/// Each chunk is checked against its own grid ([SeriesGrid::of_chunk]) and against the chunks next to it in sample
/// order: no sample held twice on two grids or at two scales, and a chunk on a new grid (a gap) after the last tick
/// of the one before and before the first tick of the one after. A damaged or forged head is refused and the
/// stretches kept stay consistent. A chunk seen again (a piece read twice) is taken once. Consecutive chunks on one
/// grid at one scale are kept as one [SeriesStretch], so a timeline holds one entry per gap or change of scale, plus
/// one per run of lost chunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesTimeline {
    period: Ratio64,
    stretches: Vec<SeriesStretch>,
}

impl SeriesTimeline {
    /// No chunks yet, for a series of `period` ticks per sample.
    pub fn new(period: Ratio64) -> Self {
        Self {
            period,
            stretches: Vec::new(),
        }
    }

    pub fn period(&self) -> Ratio64 {
        self.period
    }

    /// The stretches, in order of first sample, none overlapping.
    pub fn stretches(&self) -> &[SeriesStretch] {
        &self.stretches
    }

    /// Add the chunk `head`: its grid checked against its ticks, then against the stretches before and after it. A
    /// push before the last stretch moves the stretches after it: for many heads out of order, use [Self::from_chunks].
    pub fn push_chunk(&mut self, head: &ChunkHead) -> Result<(), SeriesError> {
        let grid = SeriesGrid::of_chunk(head, self.period)?;
        self.push(head.first_item, head.count, grid)
    }

    /// Add `count` samples from `first_item` on `grid`, checked against the stretches before and after them: what
    /// a writer that holds no chunk head yet (a summary builder) adds.
    pub fn push(
        &mut self,
        first_item: ItemNum,
        count: u32,
        grid: SeriesGrid,
    ) -> Result<(), SeriesError> {
        let (first_tick, last_tick) = grid.chunk_ticks(self.period, first_item, count)?;
        let end_item = first_item
            .checked_add(u64::from(count))
            .ok_or(SeriesError::NoSamples { first_item })?;
        let new = SeriesStretch {
            first_item,
            end_item,
            grid,
        };
        // the stretches ascend and don't overlap: `at` is the first one ending after the new samples start
        let at = self.stretches.partition_point(|s| s.end_item <= first_item);
        if let Some(s) = self.stretches.get(at)
            && s.first_item < end_item
        {
            // samples held already: a repeat when inside one stretch of the same grid and scale
            return if s.grid == grid && s.first_item <= first_item && end_item <= s.end_item {
                Ok(())
            } else {
                Err(SeriesError::Overlap { first_item })
            };
        }
        let before = at.checked_sub(1).and_then(|i| self.stretches.get(i));
        if let Some(b) = before
            && !b.grid.same_grid(&grid)
        {
            let last = self.tick_on(b, b.last_item())?;
            if !is_gap(self.period, last, first_tick) {
                return Err(SeriesError::GridTooClose { first_item });
            }
        }
        let after = self.stretches.get(at);
        if let Some(a) = after
            && !a.grid.same_grid(&grid)
        {
            let first = self.tick_on(a, a.first_item)?;
            if !is_gap(self.period, last_tick, first) {
                return Err(SeriesError::GridTooClose { first_item });
            }
        }
        let joins_before = before.is_some_and(|b| b.grid == grid && b.end_item == first_item);
        let joins_after = after.is_some_and(|a| a.grid == grid && a.first_item == end_item);
        match (joins_before, joins_after) {
            (true, true) => {
                let a = self.stretches.remove(at);
                if let Some(b) = at.checked_sub(1).and_then(|i| self.stretches.get_mut(i)) {
                    b.end_item = a.end_item;
                }
            }
            (true, false) => {
                if let Some(b) = at.checked_sub(1).and_then(|i| self.stretches.get_mut(i)) {
                    b.end_item = end_item;
                }
            }
            (false, true) => {
                if let Some(a) = self.stretches.get_mut(at) {
                    a.first_item = first_item;
                }
            }
            (false, false) => self.stretches.insert(at, new),
        }
        Ok(())
    }

    /// The timeline of a track's chunk heads in any order. They are sorted by first sample (stably) and pushed in
    /// that order, so the result doesn't depend on the order given and each push appends or merges: n log n. A head
    /// that fails a check is left out and its error returned beside the timeline, so one damaged head costs only its
    /// chunk.
    pub fn from_chunks<'h>(
        period: Ratio64,
        heads: impl IntoIterator<Item = &'h ChunkHead>,
    ) -> (Self, Vec<SeriesError>) {
        let mut heads: Vec<&ChunkHead> = heads.into_iter().collect();
        heads.sort_by_key(|h| h.first_item);
        let mut t = Self::new(period);
        let mut errors = Vec::new();
        for h in heads {
            if let Err(e) = t.push_chunk(h) {
                errors.push(e);
            }
        }
        (t, errors)
    }

    /// The stretch holding sample `index`; None when no chunk seen holds it.
    pub fn stretch_of(&self, index: ItemNum) -> Option<&SeriesStretch> {
        let at = self.stretches.partition_point(|s| s.end_item <= index);
        self.stretches.get(at).filter(|s| s.first_item <= index)
    }

    /// The grid sample `index` is on: its stretch's, or for a sample no chunk seen holds, the grid of the stretch
    /// before when the stretch after is on the same grid (a lost chunk). None before the first stretch, in a gap,
    /// or past the last.
    pub fn grid_of(&self, index: ItemNum) -> Option<&SeriesGrid> {
        let at = self.stretches.partition_point(|s| s.end_item <= index);
        let after = self.stretches.get(at)?;
        if after.first_item <= index {
            return Some(&after.grid);
        }
        let before = self.stretches.get(at.checked_sub(1)?)?;
        before.grid.same_grid(&after.grid).then_some(&before.grid)
    }

    /// Tick of sample `index`, rounded down: on its stretch's grid, or on the grid around it when it is a lost
    /// sample ([Self::grid_of]); None where no grid holds it or past i64.
    pub fn tick(&self, index: ItemNum) -> Option<i64> {
        self.grid_of(index)?.tick(self.period, index)
    }

    /// The scale of sample `index`'s stored numbers; None when no chunk seen holds it.
    pub fn scale(&self, index: ItemNum) -> Option<Magnitude> {
        self.stretch_of(index).map(|s| s.grid.scale)
    }

    /// Whether a gap comes before stretch `i`: the stretch before is on another grid.
    pub fn gap_before(&self, i: usize) -> bool {
        match (
            i.checked_sub(1).and_then(|p| self.stretches.get(p)),
            self.stretches.get(i),
        ) {
            (Some(b), Some(s)) => !b.grid.same_grid(&s.grid),
            _ => false,
        }
    }

    /// Tick of sample `index` of stretch `s`.
    fn tick_on(&self, s: &SeriesStretch, index: ItemNum) -> Result<i64, SeriesError> {
        s.grid
            .tick(self.period, index)
            .ok_or(SeriesError::TickOverflow { index })
    }
}
