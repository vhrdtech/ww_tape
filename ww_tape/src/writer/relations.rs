//! What a writer knows of its tracks' relations ([TrackDef::relations](crate::TrackDef::relations)) from the blocks it
//! places: the track definitions say which relation feeds the range index, each chunk's own relation column gives
//! the range in its index entry. Written or replayed by a resume, a block gives the same, so a resumed piece lists
//! the ranges a piece written without the crash lists.

use std::collections::BTreeMap;

use crate::times::RelationColumn;
use crate::{Chunk, TrackFacts};

/// The relations of the tracks a writer placed definitions of.
#[derive(Debug, Default)]
pub(crate) struct Relations {
    /// The relations of each track number, from the newest track definition placed
    facts: BTreeMap<u32, TrackFacts>,
}

impl Relations {
    /// Track number `number` has the relations of `facts` from here on: its definition was placed.
    pub(crate) fn define(&mut self, number: u32, facts: TrackFacts) {
        self.facts.insert(number, facts);
    }

    /// The smallest and the largest value of the range relation in chunk `c`, by the definition placed of its track;
    /// None for a track without one, or a column that doesn't read (a damaged block a resume keeps).
    pub(crate) fn range(&self, c: &Chunk<'_>) -> Option<(u64, u64)> {
        let relation = self.facts.get(&c.head().track_number)?.range?;
        RelationColumn::read(c, relation)
            .ok()
            .flatten()
            .map(|col| col.range())
    }
}
