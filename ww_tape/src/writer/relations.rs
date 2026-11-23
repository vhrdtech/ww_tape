//! What a writer knows of its tracks' relations ([TrackDef::relations](crate::TrackDef::relations)) from the blocks it
//! places: the track definitions say which relations never decrease and which one feeds the range index, each
//! chunk's own relation columns give the range in its index entry and the newest value of each monotone relation.
//! Written or replayed by a resume, a block gives the same, so a resumed piece lists the ranges a piece written
//! without the crash lists.
//!
//! A monotone relation's values never decrease along the track, across chunks and pieces too: a chunk whose first
//! value is below the newest one placed is refused. The newest values go on into the next piece of a cut; a resume
//! takes them from the chunks it keeps and from the last chunk of each track in the pieces closed before, and the
//! range relation's from the ranges in the index entries too. Where the chunk holding a track's newest values doesn't
//! read, the track is unread: its next chunk is refused ([DescribeError::RelationUnread]) rather than written
//! unchecked, but for the range relation, whose newest value the chunk's entry states.
//!
//! The caller may go on past an unread track, or past any newest value, by its own call
//! ([TapeWriter::accept_relation_break](crate::TapeWriter::accept_relation_break)): the track's next chunk is checked
//! against the value the caller gives and is marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK), in its header
//! and its index entry. A marked chunk restarts the track's values, in the writer, in a resume and in the pieces after.

use crate::TrackNum;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::index::TrackIdent;
use crate::times::RelationColumn;
use crate::{Chunk, ColumnKind, DescribeError, TrackFacts};

/// The newest value of each monotone relation of a track, from the chunks placed and the ranges of index entries.
/// Values that never decrease: the newest is the largest.
#[derive(Clone, Debug, Default)]
pub(crate) struct Newest {
    /// By track and relation, from the chunks' columns
    values: BTreeMap<(TrackIdent, u16), u64>,
    /// The range relation's, by track, from index entries: which relation it is the entries don't say
    ranges: BTreeMap<TrackIdent, u64>,
    /// Tracks whose newest values are in a chunk that doesn't read
    unread: BTreeSet<TrackIdent>,
    /// Tracks whose relations start again in these values' chunks ([FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK)):
    /// the values of the pieces before don't count
    restarted: BTreeSet<TrackIdent>,
    /// Breaks the caller accepted ([TapeWriter::accept_relation_break](crate::TapeWriter::accept_relation_break)),
    /// by track and relation: the value the track's next chunk is checked against
    breaks: BTreeMap<(TrackIdent, u16), u64>,
}

/// What [Newest] holds of one track, as it was before a chunk placed: a block with its id and other bytes takes the
/// chunk's place.
#[derive(Clone, Debug, Default)]
struct KeyState {
    values: Vec<(u16, u64)>,
    range: Option<u64>,
    unread: bool,
    restarted: bool,
    breaks: Vec<(u16, u64)>,
}

impl Newest {
    /// The newest value of relation `relation` of track `key`, `range` its track's range relation.
    fn get(&self, key: TrackIdent, relation: u16, range: Option<u16>) -> Option<u64> {
        let value = self.values.get(&(key, relation)).copied();
        let seeded = (range == Some(relation))
            .then(|| self.ranges.get(&key).copied())
            .flatten();
        value.max(seeded)
    }

    /// Take `hi`, the largest value of track `key`'s range relation in a chunk an index entry lists.
    pub(crate) fn seed_range(&mut self, key: TrackIdent, hi: u64) {
        let v = self.ranges.entry(key).or_insert(hi);
        *v = (*v).max(hi);
    }

    /// Track `key`'s relations start again at a chunk marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK):
    /// the values before it, and the breaks it takes, are gone.
    pub(crate) fn restart(&mut self, key: TrackIdent) {
        self.values.retain(|(k, _), _| *k != key);
        self.breaks.retain(|(k, _), _| *k != key);
        self.ranges.remove(&key);
        self.unread.remove(&key);
        self.restarted.insert(key);
    }

    /// Take the values of chunk `c` of track `key`, the last of the track in a piece closed before: each relation
    /// column in [STARTS](crate::times::STARTS), the codec of every monotone relation. A column that doesn't read
    /// leaves the track unread.
    pub(crate) fn seed_chunk(&mut self, key: TrackIdent, c: &Chunk<'_>) {
        self.unread.remove(&key);
        for col in c
            .columns()
            .filter(|col| col.kind == ColumnKind::Relation && col.codec == crate::times::STARTS)
        {
            match RelationColumn::open(c, col.param, |_| false) {
                Ok(Some(values)) => {
                    let v = self
                        .values
                        .entry((key, col.param))
                        .or_insert(values.range().1);
                    *v = (*v).max(values.range().1);
                }
                Ok(None) | Err(_) => {
                    self.unread.insert(key);
                }
            }
        }
    }

    /// The chunk holding track `key`'s newest values doesn't read.
    pub(crate) fn seed_unread(&mut self, key: TrackIdent) {
        self.unread.insert(key);
    }

    /// Whether these values say anything of track `key`'s chunks: values, or that they are unread.
    fn knows(&self, key: TrackIdent) -> bool {
        self.unread.contains(&key)
            || self
                .values
                .range((key, 0)..=(key, u16::MAX))
                .next()
                .is_some()
    }

    /// Take the values of `earlier`, those of pieces before: the largest of each value; a track `earlier` leaves
    /// unread is unread here unless these values know its chunks. A track these values restart takes nothing of
    /// `earlier`. The breaks of `earlier` go on.
    pub(crate) fn after(&mut self, earlier: &Self) {
        let unread: Vec<TrackIdent> = earlier
            .unread
            .iter()
            .filter(|k| !self.knows(**k) && !self.restarted.contains(*k))
            .copied()
            .collect();
        for (k, v) in &earlier.values {
            if !self.restarted.contains(&k.0) {
                let mine = self.values.entry(*k).or_insert(*v);
                *mine = (*mine).max(*v);
            }
        }
        for (k, v) in &earlier.ranges {
            if !self.restarted.contains(k) {
                self.seed_range(*k, *v);
            }
        }
        for (k, v) in &earlier.breaks {
            if !self.restarted.contains(&k.0) {
                self.breaks.entry(*k).or_insert(*v);
            }
        }
        self.unread.extend(unread);
    }

    /// What these values hold of track `key`.
    fn state(&self, key: TrackIdent) -> KeyState {
        let of = |m: &BTreeMap<(TrackIdent, u16), u64>| {
            m.range((key, 0)..=(key, u16::MAX))
                .map(|((_, r), v)| (*r, *v))
                .collect()
        };
        KeyState {
            values: of(&self.values),
            range: self.ranges.get(&key).copied(),
            unread: self.unread.contains(&key),
            restarted: self.restarted.contains(&key),
            breaks: of(&self.breaks),
        }
    }

    /// Hold `state` of track `key` again, as [Self::state] gave it.
    fn set_state(&mut self, key: TrackIdent, state: KeyState) {
        self.values.retain(|(k, _), _| *k != key);
        self.breaks.retain(|(k, _), _| *k != key);
        self.values
            .extend(state.values.into_iter().map(|(r, v)| ((key, r), v)));
        self.breaks
            .extend(state.breaks.into_iter().map(|(r, v)| ((key, r), v)));
        match state.range {
            Some(v) => self.ranges.insert(key, v),
            None => self.ranges.remove(&key),
        };
        match state.unread {
            true => self.unread.insert(key),
            false => self.unread.remove(&key),
        };
        match state.restarted {
            true => self.restarted.insert(key),
            false => self.restarted.remove(&key),
        };
    }
}

/// The relations of the tracks a writer placed definitions of, and the newest values of the monotone ones.
#[derive(Debug, Default)]
pub(crate) struct Relations {
    /// The relations of each track number, from the newest track definition placed
    facts: BTreeMap<TrackNum, TrackFacts>,
    newest: Newest,
    /// The track of the last counted chunk and what the values held of it before: a block with its id and other
    /// bytes takes its place
    undo: Option<(TrackIdent, KeyState)>,
}

impl Relations {
    /// Track number `number` has the relations of `facts` from here on: its definition was placed.
    pub(crate) fn define(&mut self, number: TrackNum, facts: TrackFacts) {
        self.facts.insert(number, facts);
    }

    /// Whether track number `number` takes records of data in caller's blocks: none of a series or a state track;
    /// a number whose definition wasn't placed may.
    pub(crate) fn takes_records(&self, number: TrackNum) -> bool {
        self.facts
            .get(&number)
            .is_none_or(|f| f.period.is_none() && f.state.is_none())
    }

    /// The newest values, for the next piece of a cut.
    pub(crate) fn newest(&self) -> &Newest {
        &self.newest
    }

    /// Go on from `newest`, the pieces' before ([Newest::after]).
    pub(crate) fn take_newest(&mut self, newest: &Newest) {
        self.newest.after(newest);
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

    /// The tile number of a grid tile's summary block with head `h`, by the definition placed of its track; None
    /// for a series' or an event tree's block.
    pub(crate) fn summary_tile(&self, h: &crate::summary::SummaryHead) -> Option<u128> {
        self.facts.get(&h.track_number)?.tile_number(&h.tile)
    }

    /// The tile number of grid chunk `c` by the definition placed of its track; None for a series or events chunk
    /// and for coordinates the grid doesn't hold (a damaged block a resume keeps).
    pub(crate) fn tile(&self, c: &Chunk<'_>) -> Option<u128> {
        let head = c.head();
        self.facts.get(&head.track_number)?.tile_number(&head.tile)
    }

    /// Track `key`'s next chunk is checked against `from` for monotone relation `relation`, not against the values
    /// before it, and is marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK).
    pub(crate) fn accept_break(&mut self, key: TrackIdent, relation: u16, from: u64) {
        self.newest.breaks.insert((key, relation), from);
    }

    /// Refuse the events chunk `c` of track `key`, whose relations `facts` describe, when a monotone relation's
    /// first value is below the newest one placed before it, or below the value of a break the caller accepted; with
    /// `replacing`, before the last counted chunk, whose place it takes. A column that doesn't read is refused before
    /// ([crate::open::relation_columns_read]). Whether the chunk is a break: one the caller accepted waits for the
    /// track.
    pub(crate) fn check(
        &self,
        key: TrackIdent,
        facts: &TrackFacts,
        c: &Chunk<'_>,
        replacing: bool,
    ) -> Result<bool, DescribeError> {
        let before;
        let newest = match &self.undo {
            Some((k, state)) if replacing && *k == key => {
                let mut n = self.newest.clone();
                n.set_state(key, state.clone());
                before = n;
                &before
            }
            _ => &self.newest,
        };
        let head = c.head();
        let unread = newest.unread.contains(&key);
        for relation in monotone(facts) {
            let from = newest.breaks.get(&(key, relation)).copied();
            // the range relation's newest value is in the unread chunk's entry
            let seeded = facts.range == Some(relation) && newest.ranges.contains_key(&key);
            if unread && !seeded && from.is_none() {
                return Err(DescribeError::RelationUnread {
                    track: head.track_number,
                    relation,
                });
            }
            let Ok(Some(col)) = RelationColumn::open(c, relation, |_| false) else {
                continue;
            };
            let before = from.or_else(|| newest.get(key, relation, facts.range));
            if before.is_some_and(|b| col.range().0 < b) {
                return Err(DescribeError::RelationDecrease {
                    track: head.track_number,
                    relation,
                    item: head.first_item,
                });
            }
        }
        Ok(newest
            .breaks
            .range((key, 0)..=(key, u16::MAX))
            .next()
            .is_some())
    }

    /// Take the newest values of the monotone relations of the counted chunk `c` of track `key` placed, by the
    /// definition placed of its track; a chunk marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK) restarts
    /// them. With `replaced`, it takes the place of the last counted chunk: that one's values are taken back first.
    pub(crate) fn placed(&mut self, key: TrackIdent, c: &Chunk<'_>, replaced: bool) {
        if replaced && let Some((k, state)) = self.undo.take() {
            self.newest.set_state(k, state);
        }
        self.undo = Some((key, self.newest.state(key)));
        if c.relation_break() {
            self.newest.restart(key);
        }
        self.take_values(key, c, None);
    }

    /// Take the newest values of the monotone relations of the events chunk `c` of track `key`, deferred: it is
    /// placed before any later block of its track, so a chunk after it is checked against it.
    pub(crate) fn deferred(&mut self, key: TrackIdent, c: &Chunk<'_>, facts: TrackFacts) {
        if c.relation_break() {
            self.newest.restart(key);
        }
        self.take_values(key, c, Some(facts));
    }

    /// Take the values of chunk `c` of track `key`, by `facts` or else the definition placed.
    fn take_values(&mut self, key: TrackIdent, c: &Chunk<'_>, facts: Option<TrackFacts>) {
        let Some(facts) = facts.or_else(|| self.facts.get(&c.head().track_number).copied()) else {
            return;
        };
        // a chunk that reads holds a column of each stored relation: the newest values are known again
        self.newest.unread.remove(&key);
        for relation in monotone(&facts) {
            let Ok(Some(col)) = RelationColumn::open(c, relation, |_| false) else {
                continue;
            };
            let hi = col.range().1;
            let v = self.newest.values.entry((key, relation)).or_insert(hi);
            *v = (*v).max(hi);
        }
    }

    /// Take a damaged chunk of track `key` a resume keeps: the range `hi` its index entry states, if any, after a
    /// restart where the entry marks a break; the track's newest values are unread.
    pub(crate) fn seed_damaged(&mut self, key: TrackIdent, hi: Option<u64>, relation_break: bool) {
        if relation_break {
            self.newest.restart(key);
        }
        if let Some(hi) = hi {
            self.newest.seed_range(key, hi);
        }
        self.newest.seed_unread(key);
    }
}

/// The relations of `facts` that keep their values in a column and never decrease, by index.
fn monotone(facts: &TrackFacts) -> impl Iterator<Item = u16> + '_ {
    (facts.stored & facts.monotone).iter()
}
