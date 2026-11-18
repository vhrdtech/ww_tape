//! What [TapeReader::verify](super::TapeReader::verify) checks past the content hash: the piece replayed block by
//! block as its writer placed them, in one pass over the blocks walked and the final index's entries in file order.
//! The index and the walk must be the same set: each entry names the start of a block walked before the finish, and
//! each block walked that its writer would list has its entries there, field for field (a chunk's with the unit hash
//! of its bytes and its range relation's range from its own column). The one block walked that may go unlisted is a
//! byte-identical copy of the listed block before it (a block written again). The blocks of data fold into their
//! tracks' hashes as the writer folded them, from the head rows' hashes, and each finish row that says its hash
//! complete must be that fold. A block that doesn't read whole and that the index lists (a damaged block a resume
//! kept) lies in bytes no block explains and stands by its entries, as the resume took it: a chunk by the unit hash
//! its entry states, else its track's hash is incomplete.
//!
//! Each block walked and each entry costs a bounded number of steps; the entries of one block are compared as sorted
//! lists. What verify holds per piece is the final index's entries, at most [VERIFY_ENTRIES_MAX].

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

/// Most final index entries [TapeReader::verify](super::TapeReader::verify) holds for one piece: 4 Mi entries,
/// about 450 MiB; a piece listing more is refused ([ReadError::VerifyEntries](super::ReadError::VerifyEntries)).
pub const VERIFY_ENTRIES_MAX: usize = 1 << 22;

/// A piece replayed block by block in file order ([Self::block], [Self::raw]), then checked against its finish rows
/// ([Self::check]).
pub(super) struct Replay {
    /// Where the next step starts
    at: u64,
    /// Where the walk ends: the finish block's offset
    end: u64,
    /// The final index's entries in file order
    listed: Vec<IndexEntry>,
    /// The first entry of `listed` not yet met
    next: usize,
    /// The end of the last damaged block the index lists: no block walked starts before it
    damaged_end: u64,
    /// The bytes of the last listed block walked, which a block written again copies
    prev: Vec<u8>,
    /// The index and the walk are still the same set
    same: bool,
    /// Steps taken: blocks walked, entries met and compared
    steps: u64,
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
    /// A replay of a piece whose head rows are `starts`, whose final index lists `listed` (in file order) and whose
    /// finish starts at `end`.
    pub(super) fn new(starts: &[TrackStart], listed: Vec<IndexEntry>, end: u64) -> Self {
        let mut r = Replay {
            at: 0,
            end,
            listed,
            next: 0,
            damaged_end: 0,
            prev: Vec::new(),
            same: true,
            steps: 0,
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
        r
    }

    /// The steps taken so far.
    pub(super) fn steps(&self) -> u64 {
        self.steps
    }

    /// The entries listed at `at`, taken: none when the next entry starts later.
    fn take_listed(&mut self, at: u64) -> core::ops::Range<usize> {
        self.skip_before(at);
        let from = self.next;
        while self.listed.get(self.next).is_some_and(|e| e.offset == at) {
            self.next = self.next.saturating_add(1);
            self.steps = self.steps.saturating_add(1);
        }
        from..self.next
    }

    /// Step over the entries before `at` not yet met: each starts inside a block walked, so the index and the walk
    /// differ.
    fn skip_before(&mut self, at: u64) {
        if self.listed.get(self.next).is_some_and(|e| e.offset < at) {
            self.same = false;
            self.next = self.listed.partition_point(|e| e.offset < at);
        }
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

    /// The good block `block` at the place walked, `chunk` it read as a chunk with its payload hash. A block the index
    /// lists there must be listed with the entries its writer made of it; a block its writer would list must be
    /// listed, unless it is a byte-identical copy of the listed block before it (written again), which adds nothing.
    pub(super) fn block(&mut self, block: &[u8], chunk: Option<(Chunk<'_>, [u8; 32])>) {
        let at = self.at;
        self.at = at.saturating_add(wide(block.len()));
        self.steps = self.steps.saturating_add(1);
        if at < self.damaged_end {
            // a block inside one the index lists as damaged: the writer took the damaged one
            self.same = false;
        }
        let listed = self.take_listed(at);
        let Ok(b) = Block::read_trusted(block) else {
            self.same &= listed.is_empty();
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
        let mut expected: Vec<IndexEntry> = match &chunk {
            Some((c, _)) => IndexEntry::of_chunk(at, block.len(), &c.head())
                .map(|e| IndexEntry {
                    unit,
                    range,
                    relation_break: c.relation_break(),
                    ..e
                })
                .into_iter()
                .collect(),
            None if matches!(b.kind, BlockKind::Events | BlockKind::Series) => Vec::new(),
            None => IndexEntry::all_of_block(at, &b),
        };
        if listed.is_empty() {
            let copy = !self.prev.is_empty() && self.prev == block;
            self.same &= expected.is_empty() || copy;
            return;
        }
        let mut listed = self.listed.get(listed).unwrap_or_default().to_vec();
        self.steps = self
            .steps
            .saturating_add(wide(listed.len().saturating_add(expected.len())));
        if !same_entries(&mut listed, &mut expected) {
            self.same = false;
            return;
        }
        self.prev.clear();
        self.prev.extend_from_slice(block);
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

    /// `n` bytes no block explains at the place walked. Each block the index lists in them is a damaged block its
    /// writer's resume kept: its entries stand for it as they did for the resume. It must end before the finish.
    pub(super) fn raw(&mut self, n: usize) {
        let at = self.at;
        let to = at.saturating_add(wide(n));
        self.at = to;
        self.steps = self.steps.saturating_add(1);
        self.skip_before(at);
        while let Some(x) = self
            .listed
            .get(self.next)
            .map(|e| e.offset)
            .filter(|x| *x < to)
        {
            let group = self.take_listed(x);
            let entries = self.listed.get(group).unwrap_or_default().to_vec();
            for e in &entries {
                let block_end = e.offset.saturating_add(u64::from(e.len));
                if block_end > self.end {
                    self.same = false;
                }
                self.damaged_end = self.damaged_end.max(block_end);
            }
            self.damaged(&entries);
        }
    }

    /// The entries `entries` of a damaged block kept stand for it.
    fn damaged(&mut self, entries: &[IndexEntry]) {
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

    /// Whether the piece's final index and its walk are the same set (every entry met, at a block walked or a damaged
    /// block in bytes no block explains, before the finish), and each finish row in `rows` states the hash its track's
    /// listed blocks fold to, incomplete exactly where the head or a damaged block leaves units out.
    pub(super) fn check(self, rows: &[TrackRange]) -> bool {
        if !self.same || self.next != self.listed.len() {
            return false;
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

/// Whether two lists of the entries of one block hold the same entries; both are sorted.
fn same_entries(a: &mut [IndexEntry], b: &mut [IndexEntry]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.sort_unstable_by_key(entry_key);
    b.sort_unstable_by_key(entry_key);
    a == b
}

/// What orders the entries of one block.
#[allow(clippy::type_complexity, reason = "the fields of an entry in order")]
fn entry_key(e: &IndexEntry) -> (u64, u32, u32, u64, u64, i64, i64, i64, [u8; 32], u8) {
    (
        e.track_id,
        e.track_number,
        e.second,
        e.first_item,
        e.count,
        e.first_tick,
        e.last_tick,
        e.latest_end,
        e.unit.0,
        e.kind.to_u8(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockId, BlockWriter, ClockTime, SessionTag};

    fn records(id: BlockId, n: i64) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[1]).unwrap();
        b.push(9, ClockTime::new(1, n), &[&[n as u8; 30]]).unwrap();
        let len = b.finish();
        buf[..len].to_vec()
    }

    /// The steps of a replay of `n` record blocks with every other one damaged (bytes no block explains), each listed
    /// at its offset plus `off`, and whether the replay holds; with the entries listed.
    fn replay(n: u32, off: u64) -> (u64, bool, u64) {
        let mut listed = Vec::new();
        let mut blocks = Vec::new();
        let mut at = 0u64;
        for k in 0..n {
            let b = records(
                BlockId::new(SessionTag::from_u32(1), u64::from(k)),
                i64::from(k),
            );
            let block = Block::read(&b).unwrap();
            listed.extend(IndexEntry::all_of_block(at + off, &block));
            at += wide(b.len());
            blocks.push(b);
        }
        let entries = wide(listed.len());
        let mut r = Replay::new(&[], listed, at + off);
        for (k, b) in blocks.iter().enumerate() {
            match k % 2 {
                0 => r.block(b, None),
                _ => r.raw(b.len()),
            }
        }
        (r.steps(), r.check(&[]), entries)
    }

    /// A replay costs a bounded number of steps per block walked and per entry listed, whatever the index lists:
    /// the blocks at their offsets, or every entry a byte past its block's start (no block walked starts there).
    #[test]
    fn a_replay_takes_steps_linear_in_blocks_and_entries() {
        for off in [0, 1] {
            let (small, holds, entries) = replay(1000, off);
            assert_eq!(holds, off == 0);
            assert!(small <= 3 * (1000 + entries), "{small}");
            let (large, _, entries) = replay(16000, off);
            assert!(large <= 3 * (16000 + entries), "{large}");
            assert!(large <= 17 * small, "{small} {large}");
        }
    }
}
