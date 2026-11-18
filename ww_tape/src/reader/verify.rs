//! What [TapeReader::verify](super::TapeReader::verify) checks past the content hash: the piece replayed block by
//! block as its writer placed them. Each block walked gives the index entries its writer listed for it (a chunk's
//! with the unit hash of its bytes and its range relation's range from its own column), compared with the final
//! index's: every entry names the block at its offset, field for field, and every block of data has its entries. The
//! blocks of data fold into their tracks' hashes as the writer folded them, from the head rows' hashes, and each
//! finish row that says its hash complete must be that fold. A block that doesn't read whole and that the index lists
//! (a damaged block a resume kept) stands by its entries, as the resume took it: a chunk by the unit hash its entry
//! states, else its track's hash is incomplete.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use shrink_wrap::DeserializeShrinkWrap;

use crate::index::{IndexEntry, STATE_TRACK_ID, is_structural};
use crate::piece::{OwnerId, TrackHash, TrackRange, TrackStart, UnitHash};
use crate::times::RelationColumn;
use crate::{Block, BlockKind, Chunk, DESCRIBE_TRACK, Describe, FLAG_REPEAT, Slot, TrackFacts};

use super::wide;

/// A track by its id and owner.
type Key = (u64, OwnerId);

/// A piece replayed block by block in file order ([Self::step]), then checked against its index and finish rows
/// ([Self::check]).
pub(super) struct Replay {
    /// Where the next step starts
    at: u64,
    /// The final index's entries by offset
    listed: BTreeMap<u64, Vec<IndexEntry>>,
    /// The entries the writer listed for each block walked, by offset
    expected: BTreeMap<u64, Vec<IndexEntry>>,
    /// Where each block walked starts and ends
    blocks: Vec<(u64, u64)>,
    /// Track numbers defined in the piece
    numbers: BTreeMap<u32, Key>,
    /// Track numbers the head carries
    carried: BTreeMap<u32, Key>,
    /// The relations of each track number defined in the piece
    facts: BTreeMap<u32, TrackFacts>,
    /// Each track's hash so far
    hashes: BTreeMap<Key, TrackHash>,
    /// Tracks a block of data was folded into
    touched: BTreeSet<Key>,
    /// Tracks whose hash leaves units out: by the head, or a damaged block kept
    incomplete: BTreeSet<Key>,
}

impl Replay {
    /// A replay of a piece whose head rows are `starts` and whose final index lists `listed`.
    pub(super) fn new(starts: &[TrackStart], listed: &[IndexEntry]) -> Self {
        let mut r = Replay {
            at: 0,
            listed: BTreeMap::new(),
            expected: BTreeMap::new(),
            blocks: Vec::new(),
            numbers: BTreeMap::new(),
            carried: BTreeMap::new(),
            facts: BTreeMap::new(),
            hashes: BTreeMap::new(),
            touched: BTreeSet::new(),
            incomplete: BTreeSet::new(),
        };
        for t in starts {
            let key = (t.track_id, t.owner);
            r.carried.insert(t.track_number, key);
            r.hashes.insert(key, t.hash);
            if t.incomplete {
                r.incomplete.insert(key);
            }
        }
        for e in listed {
            r.listed.entry(e.offset).or_default().push(*e);
        }
        r
    }

    /// The track of number `number`, its id `id` when the block says it, as the writer keyed it: by the definition
    /// placed in the piece, else by the head; a chunk whose number names no track of its id is of the piece's owner.
    fn key_of(&self, number: u32, id: Option<u64>) -> Option<Key> {
        let fits = |k: &&Key| id.is_none_or(|id| k.0 == id);
        self.numbers
            .get(&number)
            .filter(fits)
            .or_else(|| self.carried.get(&number).filter(fits))
            .copied()
            .or_else(|| id.map(|id| (id, OwnerId::NONE)))
    }

    /// Fold `unit` into track `key`'s hash.
    fn fold(&mut self, key: Key, unit: impl FnOnce(&TrackHash) -> TrackHash) {
        let before = self.hashes.get(&key).copied();
        let h = unit(&before.unwrap_or(TrackHash::START));
        self.hashes.insert(key, h);
        self.touched.insert(key);
    }

    /// The good block `block` at the place walked, `chunk` it read as a chunk with its payload hash. Only a block the
    /// index lists is one its writer took: a good block it doesn't list is a block written again with the same bytes,
    /// one another took the place of, or bytes a resume kept as junk, and adds nothing.
    pub(super) fn block(&mut self, block: &[u8], chunk: Option<(Chunk<'_>, [u8; 32])>) {
        let at = self.at;
        let end = at.saturating_add(wide(block.len()));
        self.at = end;
        self.blocks.push((at, end));
        if !self.listed.contains_key(&at) {
            return;
        }
        let Ok(b) = Block::read_trusted(block) else {
            return;
        };
        let repeat = b.kind == BlockKind::Records && b.flags & FLAG_REPEAT != 0;
        let counted = !repeat && !is_structural(b.kind);
        let unit = chunk.as_ref().map_or(UnitHash::NONE, |(c, payload)| {
            TrackHash::chunk_unit(c, payload)
        });
        let range = chunk.as_ref().and_then(|(c, _)| {
            let relation = self.facts.get(&c.head().track_number)?.range?;
            RelationColumn::open(c, relation, |_| false)
                .ok()
                .flatten()
                .map(|col| col.range())
        });
        let entries: Vec<IndexEntry> = match &chunk {
            Some((c, _)) => IndexEntry::of_chunk(at, block.len(), &c.head())
                .map(|e| IndexEntry { unit, range, ..e })
                .into_iter()
                .collect(),
            None if matches!(b.kind, BlockKind::Events | BlockKind::Series) => Vec::new(),
            None => IndexEntry::all_of_block(at, &b),
        };
        self.expected.insert(at, entries);
        if counted && let Some((c, _)) = &chunk {
            let head = c.head();
            if let Some(key) = self.key_of(head.track_number, Some(head.track_id)) {
                self.fold(key, |h| h.then(&unit));
            }
        }
        self.learn(&b, counted);
    }

    /// What the records of block `b` say, as the writer took them: each definition of a track names it from here
    /// on; with `tally`, each record of data goes into its track's hash.
    fn learn(&mut self, b: &Block<'_>, tally: bool) {
        if b.kind != BlockKind::Records || b.chunk().is_some() {
            return;
        }
        let mut buf = Vec::new();
        let Ok(b) = b.decode(&mut buf) else {
            return;
        };
        let repeat = b.is_repeat();
        for r in b.records().flatten() {
            if r.track != DESCRIBE_TRACK && !repeat {
                if tally && let Some(key) = self.key_of(r.track, None) {
                    let ticks = r.at.map_or(0, |a| a.ticks);
                    self.fold(key, |h| h.record(ticks, r.item));
                }
                continue;
            }
            if let Some(Slot::Track { .. }) = Slot::of(r.track, r.item)
                && let Ok(Describe::Track(t)) = Describe::from_ww_bytes(r.item)
            {
                self.numbers.insert(t.number.0, (t.id(), OwnerId(t.owner)));
                self.facts.insert(t.number.0, TrackFacts::of(&t));
            }
        }
    }

    /// `n` bytes no block explains at the place walked. Where the index lists a block there, a damaged block its
    /// writer's resume kept, its entries stand for it as they did for the resume.
    pub(super) fn raw(&mut self, n: usize) {
        let at = self.at;
        self.at = at.saturating_add(wide(n));
        if self.blocks.last().is_some_and(|(_, end)| *end > at) {
            return;
        }
        let Some(entries) = self.listed.get(&at).cloned() else {
            return;
        };
        for e in entries.iter().filter(|e| !e.repeat) {
            match e.kind {
                BlockKind::Events | BlockKind::Series => {
                    if let Some(key) = self.key_of(e.track_number, Some(e.track_id)) {
                        match e.unit == UnitHash::NONE {
                            true => {
                                self.touched.insert(key);
                                self.incomplete.insert(key);
                            }
                            false => self.fold(key, |h| h.then(&e.unit)),
                        }
                    }
                }
                // the records of a damaged block of data: each track that may hold records leaves them out
                BlockKind::Records if e.track_id != STATE_TRACK_ID => {
                    let takes = |n: &u32| {
                        self.facts
                            .get(n)
                            .is_none_or(|f| f.period.is_none() && f.state.is_none())
                    };
                    let keys: Vec<Key> = self
                        .carried
                        .iter()
                        .chain(&self.numbers)
                        .filter(|(n, _)| takes(n))
                        .map(|(_, k)| *k)
                        .collect();
                    for key in keys {
                        self.touched.insert(key);
                        self.incomplete.insert(key);
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether the piece's final index lists, at each block walked it lists, exactly the entries its writer made of
    /// that block; lists a block that doesn't read whole (a damaged block kept) only where no block walked holds its
    /// start; and each finish row in `rows` states the hash its track's listed blocks fold to, incomplete exactly where
    /// the head or a damaged block leaves units out. A good block the index doesn't list adds to no track: an entry
    /// taken out of the index changes the fold, which the finish row then doesn't state.
    pub(super) fn check(self, rows: &[TrackRange]) -> bool {
        let inside_a_block = |at: u64| self.blocks.iter().any(|(s, e)| *s < at && at < *e);
        for (at, listed) in &self.listed {
            let holds = match self.expected.get(at) {
                Some(expected) => same_entries(listed, expected),
                None => !inside_a_block(*at),
            };
            if !holds {
                return false;
            }
        }
        let mut rowed = BTreeSet::new();
        for r in rows {
            let key = (r.track_id, r.owner);
            rowed.insert(key);
            let incomplete = self.incomplete.contains(&key);
            if incomplete != r.incomplete {
                return false;
            }
            let hash = self.hashes.get(&key).copied().unwrap_or(TrackHash::START);
            if !r.incomplete && hash != r.hash {
                return false;
            }
        }
        self.touched.iter().all(|k| rowed.contains(k))
    }
}

/// Whether two lists of the entries of one block hold the same entries.
fn same_entries(a: &[IndexEntry], b: &[IndexEntry]) -> bool {
    a.len() == b.len() && a.iter().all(|e| b.contains(e)) && b.iter().all(|e| a.contains(e))
}
