//! What a writer knows of its tracks' relations ([TrackDef::relations](crate::TrackDef::relations)) from the blocks it
//! places: the track definitions say which relations never decrease and which one feeds the range index, each
//! chunk's own relation columns give the range in its index entry and the newest value of each monotone relation.
//! Written or replayed by a resume, a block gives the same, so a resumed piece lists the ranges a piece written
//! without the crash lists.
//!
//! A monotone relation's values never decrease along the track, across chunks and pieces too: a chunk whose first
//! value is below the newest one placed is refused. The newest values go on into the next piece of a cut; a resume
//! takes them from the chunks it keeps, and from the ranges in the index entries of a chunk that doesn't read and of
//! the pieces finished before.

use std::collections::BTreeMap;

use crate::index::TrackKey;
use crate::times::RelationColumn;
use crate::{Chunk, DescribeError, TrackFacts};

/// The newest value of each monotone relation of a track, from the chunks placed and the ranges of index entries.
/// Values that never decrease: the newest is the largest.
#[derive(Clone, Debug, Default)]
pub(crate) struct Newest {
    /// By track and relation, from the chunks' columns
    values: BTreeMap<(TrackKey, u16), u64>,
    /// The range relation's, by track, from index entries: which relation it is the entries don't say
    ranges: BTreeMap<TrackKey, u64>,
}

impl Newest {
    /// The newest value of relation `relation` of track `key`, `range` its track's range relation.
    fn get(&self, key: TrackKey, relation: u16, range: Option<u16>) -> Option<u64> {
        let value = self.values.get(&(key, relation)).copied();
        let seeded = (range == Some(relation))
            .then(|| self.ranges.get(&key).copied())
            .flatten();
        value.max(seeded)
    }

    /// Take `hi`, the largest value of track `key`'s range relation in a chunk an index entry lists.
    pub(crate) fn seed_range(&mut self, key: TrackKey, hi: u64) {
        let v = self.ranges.entry(key).or_insert(hi);
        *v = (*v).max(hi);
    }

    /// Take every value of `other` as well.
    pub(crate) fn merge(&mut self, other: &Newest) {
        for (k, v) in &other.values {
            let mine = self.values.entry(*k).or_insert(*v);
            *mine = (*mine).max(*v);
        }
        for (k, v) in &other.ranges {
            self.seed_range(*k, *v);
        }
    }
}

/// The relations of the tracks a writer placed definitions of, and the newest values of the monotone ones.
#[derive(Debug, Default)]
pub(crate) struct Relations {
    /// The relations of each track number, from the newest track definition placed
    facts: BTreeMap<u32, TrackFacts>,
    newest: Newest,
    /// The values the last counted chunk changed, as they were before it: a block with its id and other bytes
    /// takes its place
    undo: Vec<((TrackKey, u16), Option<u64>)>,
}

impl Relations {
    /// Track number `number` has the relations of `facts` from here on: its definition was placed.
    pub(crate) fn define(&mut self, number: u32, facts: TrackFacts) {
        self.facts.insert(number, facts);
    }

    /// The newest values, for the next piece of a cut.
    pub(crate) fn newest(&self) -> &Newest {
        &self.newest
    }

    /// Go on from `newest` (the piece before's, or seeds a resume read).
    pub(crate) fn take_newest(&mut self, newest: &Newest) {
        self.newest.merge(newest);
    }

    /// The smallest and the largest value of the range relation in chunk `c`, by the definition placed of its track;
    /// None for a track without one, or a column that doesn't read (a damaged block a resume keeps).
    pub(crate) fn range(&self, c: &Chunk<'_>) -> Option<(u64, u64)> {
        let relation = self.facts.get(&c.head().track_number)?.range?;
        RelationColumn::open(c, relation, |_| false)
            .ok()
            .flatten()
            .map(|col| col.range())
    }

    /// Refuse the events chunk `c` of track `key`, whose relations `facts` describe, when a monotone relation's
    /// first value is below the newest one placed before it; with `replacing`, before the last counted chunk, whose
    /// place it takes. A column that doesn't read is refused before ([crate::open::relation_columns_read]).
    pub(crate) fn check(
        &self,
        key: TrackKey,
        facts: &TrackFacts,
        c: &Chunk<'_>,
        replacing: bool,
    ) -> Result<(), DescribeError> {
        let head = c.head();
        for relation in monotone(facts) {
            let Ok(Some(col)) = RelationColumn::open(c, relation, |_| false) else {
                continue;
            };
            let before = if replacing {
                self.before_last(key, relation, facts.range)
            } else {
                self.newest.get(key, relation, facts.range)
            };
            if before.is_some_and(|b| col.range().0 < b) {
                return Err(DescribeError::RelationDecrease {
                    track: head.track_number,
                    relation,
                    item: head.first_item,
                });
            }
        }
        Ok(())
    }

    /// The newest value of a relation before the last counted chunk.
    fn before_last(&self, key: TrackKey, relation: u16, range: Option<u16>) -> Option<u64> {
        match self.undo.iter().find(|(k, _)| *k == (key, relation)) {
            Some((_, before)) => {
                let seeded = (range == Some(relation))
                    .then(|| self.newest.ranges.get(&key).copied())
                    .flatten();
                (*before).max(seeded)
            }
            None => self.newest.get(key, relation, range),
        }
    }

    /// Take the newest values of the monotone relations of the counted chunk `c` of track `key` placed, by the
    /// definition placed of its track. With `replaced`, it takes the place of the last counted chunk: that one's
    /// values are taken back first.
    pub(crate) fn placed(&mut self, key: TrackKey, c: &Chunk<'_>, replaced: bool) {
        if replaced {
            for (k, before) in self.undo.drain(..).rev() {
                match before {
                    Some(v) => self.newest.values.insert(k, v),
                    None => self.newest.values.remove(&k),
                };
            }
        }
        self.undo = self.take_values(key, c, None);
    }

    /// Take the newest values of the monotone relations of the events chunk `c` of track `key`, deferred: it is
    /// placed before any later block of its track, so a chunk after it is checked against it.
    pub(crate) fn deferred(&mut self, key: TrackKey, c: &Chunk<'_>, facts: TrackFacts) {
        self.take_values(key, c, Some(facts));
    }

    /// Take the values of chunk `c` of track `key`, by `facts` or else the definition placed; the values as they were.
    fn take_values(
        &mut self,
        key: TrackKey,
        c: &Chunk<'_>,
        facts: Option<TrackFacts>,
    ) -> Vec<((TrackKey, u16), Option<u64>)> {
        let mut before = Vec::new();
        let Some(facts) = facts.or_else(|| self.facts.get(&c.head().track_number).copied()) else {
            return before;
        };
        for relation in monotone(&facts) {
            let Ok(Some(col)) = RelationColumn::open(c, relation, |_| false) else {
                continue;
            };
            let hi = col.range().1;
            let k = (key, relation);
            let was = self.newest.values.get(&k).copied();
            before.push((k, was));
            self.newest.values.insert(k, was.map_or(hi, |b| b.max(hi)));
        }
        before
    }

    /// Take the range `hi` of an index entry of track `key` (a damaged chunk a resume keeps).
    pub(crate) fn seed_range(&mut self, key: TrackKey, hi: u64) {
        self.newest.seed_range(key, hi);
    }
}

/// The relations of `facts` that keep their values in a column and never decrease, by index.
fn monotone(facts: &TrackFacts) -> impl Iterator<Item = u16> + '_ {
    (0..crate::RELATIONS_MAX)
        .filter_map(|i| u16::try_from(i).ok())
        .filter(|i| {
            1u16.checked_shl(u32::from(*i))
                .is_some_and(|bit| facts.stored & facts.monotone & bit != 0)
        })
}
