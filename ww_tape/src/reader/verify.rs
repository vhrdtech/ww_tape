//! What [TapeReader::verify](super::TapeReader::verify) checks past the content hash: the piece replayed block by
//! block as its writer placed them, in one pass over the blocks walked and the final index's entries in file order.
//! The index and the walk must be the same set: each entry names the start of a block walked before the close, and
//! each block walked that its writer would list has its entries there, field for field (a chunk's with the unit hash
//! of its bytes and its range relation's range from its own column). The one block walked that may go unlisted is a
//! byte-identical copy of the listed block before it (a block written again). The blocks of data fold into their
//! tracks' hashes as the writer folded them, from the head rows' hashes, and each close row that says its hash
//! complete must be that fold. A block that doesn't read whole and that the index lists (a damaged block a resume
//! kept) lies in bytes no block explains and stands by its entries, as the resume took it: a chunk by the unit hash
//! its entry states, else its track's hash is incomplete.
//!
//! Each block walked and each entry costs a bounded number of steps; the entries of one block are compared as sorted
//! lists. The final index is read as it is walked: its track lists, each in file order, merged into one stream in
//! file order ([Lists]), so verify holds one entry per track list and the entries of one block, whatever the piece's
//! size.

use alloc::collections::{BTreeMap, BTreeSet, BinaryHeap};
use alloc::vec::Vec;
use core::cmp::Reverse;

use shrink_wrap::DeserializeShrinkWrap;

use crate::index::{ENTRY_LEN, IndexEntry, STATE_TRACK_ID, in_file_order, is_structural};
use crate::piece::{OwnerId, TrackHash, TrackRange, TrackStart, UnitHash};
use crate::times::RelationColumn;
use crate::{Block, BlockKind, Chunk, DESCRIBE_TRACK, Describe, FLAG_REPEAT, Slot, TrackFacts};

use super::wide;

/// A track by its id and owner.
type Key = (u64, OwnerId);

/// Most track lists of a final index [TapeReader::verify](super::TapeReader::verify) takes: one per track a piece
/// takes data of ([TRACKS_PER_PIECE_MAX](crate::piece::TRACKS_PER_PIECE_MAX)), the state records' and the records of data's.
/// Verify holds one entry per list, so this bounds what it holds whatever the piece's size.
pub const VERIFY_LISTS_MAX: usize = crate::piece::TRACKS_PER_PIECE_MAX.saturating_add(2);

/// What [TapeReader::verify](super::TapeReader::verify) found and what it cost.
#[derive(Copy, Clone, Debug, Default)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the cost is read by the tests' bounds")
)]
pub(crate) struct Verified {
    /// The piece holds up
    pub(crate) holds: bool,
    /// Steps the replay took: blocks walked, entries met and compared
    pub(crate) steps: u64,
    /// Most entries held at once: one per track list, and the entries of one block
    pub(crate) held: usize,
}

/// The final index's entries, met in file order.
pub(super) trait Listed {
    /// The offset of the next entry; None past the last, or once the entries don't read in file order.
    fn peek(&self) -> Option<u64>;
    /// The next entry, taken.
    fn take(&mut self) -> Option<IndexEntry>;
    /// Whether every entry read in file order, each within the piece.
    fn good(&self) -> bool;
}

/// A track list of a final index: where its next entry is and how many are left.
struct Cursor {
    /// Where its next entry starts in the source
    at: u64,
    /// Its entries not yet read
    left: u32,
    /// Bytes from one entry to the next
    step: u64,
    /// Its next entry, read
    next: Option<IndexEntry>,
}

/// The entries of a final index's track lists merged into file order, read one at a time by `read` (the first
/// [ENTRY_LEN] bytes at a place in the source): it holds one entry per list. Each entry is checked as every read of
/// an index checks it, the merged stream must be in file order ([in_file_order]), and `fits` must take each entry
/// (within the piece); else the stream ends and isn't [good](Listed::good).
pub(super) struct Lists<F, G, E> {
    cursors: Vec<Cursor>,
    /// The next offset of each list, by list
    heap: BinaryHeap<Reverse<(u64, usize)>>,
    read: F,
    fits: G,
    prev: Option<IndexEntry>,
    bad: bool,
    error: Option<E>,
}

impl<F, G, E> Lists<F, G, E>
where
    F: FnMut(u64, &mut [u8; ENTRY_LEN]) -> Result<(), E>,
    G: Fn(&IndexEntry) -> bool,
{
    /// A merge of no lists yet.
    pub(super) fn new(read: F, fits: G) -> Self {
        Self {
            cursors: Vec::new(),
            heap: BinaryHeap::new(),
            read,
            fits,
            prev: None,
            bad: false,
            error: None,
        }
    }

    /// Add the list of `count` entries from `at` in the source, `step` bytes apart.
    pub(super) fn add(&mut self, at: u64, count: u32, step: u64) {
        let i = self.cursors.len();
        self.cursors.push(Cursor {
            at,
            left: count,
            step,
            next: None,
        });
        self.advance(i, None);
    }

    /// The lists held: one entry each.
    pub(super) fn held(&self) -> usize {
        self.cursors.len()
    }

    /// The error of the source, if a read failed.
    pub(super) fn take_error(&mut self) -> Option<E> {
        self.error.take()
    }

    /// Read list `i`'s next entry, after `prev`, and file it by its offset; an entry before `prev` is out of order.
    fn advance(&mut self, i: usize, prev: Option<IndexEntry>) {
        let Some(c) = self.cursors.get_mut(i) else {
            return;
        };
        if c.left == 0 || self.bad {
            return;
        }
        let mut b = [0u8; ENTRY_LEN];
        if let Err(e) = (self.read)(c.at, &mut b) {
            self.error = Some(e);
            self.bad = true;
            return;
        }
        c.left = c.left.saturating_sub(1);
        c.at = c.at.saturating_add(c.step);
        match IndexEntry::read(&b) {
            Ok(e) if prev.is_none_or(|p| p.offset <= e.offset) => {
                self.heap.push(Reverse((e.offset, i)));
                c.next = Some(e);
            }
            _ => self.bad = true,
        }
    }
}

impl<F, G, E> Listed for Lists<F, G, E>
where
    F: FnMut(u64, &mut [u8; ENTRY_LEN]) -> Result<(), E>,
    G: Fn(&IndexEntry) -> bool,
{
    fn peek(&self) -> Option<u64> {
        match self.bad {
            true => None,
            false => self.heap.peek().map(|Reverse((at, _))| *at),
        }
    }

    fn take(&mut self) -> Option<IndexEntry> {
        if self.bad {
            return None;
        }
        let Reverse((_, i)) = self.heap.pop()?;
        let e = self.cursors.get_mut(i).and_then(|c| c.next.take())?;
        self.advance(i, Some(e));
        if !(self.fits)(&e) || !in_file_order(self.prev.iter().chain([&e])) {
            self.bad = true;
            return None;
        }
        self.prev = Some(e);
        Some(e)
    }

    fn good(&self) -> bool {
        !self.bad
    }
}

/// A piece replayed block by block in file order ([Self::block], [Self::raw]), then checked against its close rows
/// ([Self::check]).
pub(super) struct Replay<L> {
    /// Where the next step starts
    at: u64,
    /// Where the walk ends: the close record's offset
    end: u64,
    /// The final index's entries in file order
    listed: L,
    /// Most entries of one block held at once
    group_max: usize,
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
    /// The top of each monotone relation of each track so far, by relation index
    tops: BTreeMap<(Key, u16), u64>,
    /// The first chunk whose monotone relation goes below the one before without a break: its offset, track id and
    /// relation (None: the range relation, from a damaged chunk's entry)
    order: Option<(u64, u64, Option<u16>)>,
}

impl<L: Listed> Replay<L> {
    /// A replay of a piece whose head rows are `starts`, whose final index lists `listed` (in file order) and whose
    /// close starts at `end`.
    pub(super) fn new(starts: &[TrackStart], listed: L, end: u64) -> Self {
        let mut r = Self {
            at: 0,
            end,
            listed,
            group_max: 0,
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
            tops: BTreeMap::new(),
            order: None,
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

    /// The first chunk whose monotone relation goes below the chunk before it of its track without a break marked: its
    /// offset in the piece, its track id and the relation (None: the range relation, by a damaged chunk's entry).
    pub(super) fn order(&self) -> Option<(u64, u64, Option<u16>)> {
        self.order
    }

    /// Take relation `relation`'s values `lo..=hi` of the chunk of track `key` at `at`, after the track's chunks before.
    fn monotone(&mut self, at: u64, key: Key, relation: u16, (lo, hi): (u64, u64)) {
        let top = self.tops.insert((key, relation), hi);
        if top.is_some_and(|t| lo < t) && self.order.is_none() {
            self.order = Some((at, key.0, Some(relation)));
        }
    }

    /// A chunk of track `key` marked a break: its monotone relations start again.
    fn restart(&mut self, key: Key) {
        self.tops.retain(|(k, _), _| *k != key);
    }

    /// Most entries of one block held at once.
    pub(super) fn group_max(&self) -> usize {
        self.group_max
    }

    /// The final index's entries.
    pub(super) fn listed(&mut self) -> &mut L {
        &mut self.listed
    }

    /// The entries listed at `at`, taken, at most `max` of them held: the rest are taken and dropped, and the index
    /// and the walk differ. None when the next entry starts later.
    fn take_listed(&mut self, at: u64, max: usize) -> Vec<IndexEntry> {
        self.skip_before(at);
        let mut group = Vec::new();
        while self.listed.peek() == Some(at) {
            self.steps = self.steps.saturating_add(1);
            let Some(e) = self.listed.take() else {
                break;
            };
            match group.len() < max {
                true => group.push(e),
                false => self.same = false,
            }
        }
        self.group_max = self.group_max.max(group.len());
        group
    }

    /// Step over the entries before `at` not yet met: each starts inside a block walked, so the index and the walk
    /// differ.
    fn skip_before(&mut self, at: u64) {
        while self.listed.peek().is_some_and(|x| x < at) {
            self.same = false;
            self.steps = self.steps.saturating_add(1);
            self.listed.take();
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
        let Ok(b) = Block::read_trusted(block) else {
            // an entry here is taken and dropped: the index and the walk differ
            self.take_listed(at, 0);
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
        let mut listed = self.take_listed(at, expected.len().saturating_add(1));
        if listed.is_empty() {
            let copy = !self.prev.is_empty() && self.prev == block;
            self.same &= expected.is_empty() || copy;
            return;
        }
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
                if head.kind == BlockKind::Events {
                    self.order_of(at, key, c);
                }
            }
        }
        self.learn(&b, counted);
    }

    /// Take the monotone relations of the counted events chunk `c` of track `key` at `at`: each must start at or above
    /// its top in the track's chunks before, but in a chunk marked a break.
    fn order_of(&mut self, at: u64, key: Key, c: &Chunk<'_>) {
        if c.relation_break() {
            self.restart(key);
        }
        let Some(facts) = self.facts.get(&c.head().track_number).copied() else {
            return;
        };
        for relation in (0..crate::RELATIONS_MAX).filter_map(|i| u16::try_from(i).ok()) {
            let monotone = 1u16
                .checked_shl(u32::from(relation))
                .is_some_and(|bit| facts.stored & facts.monotone & bit != 0);
            if !monotone {
                continue;
            }
            match RelationColumn::open(c, relation, |_| false) {
                Ok(Some(col)) => self.monotone(at, key, relation, col.range()),
                // a column that doesn't read says nothing of its values: the next chunk can't be checked against it
                _ => {
                    self.tops.remove(&(key, relation));
                }
            }
        }
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
    /// writer's resume kept: its entries stand for it as they did for the resume. It must end before the close.
    pub(super) fn raw(&mut self, n: usize) {
        let at = self.at;
        let to = at.saturating_add(wide(n));
        self.at = to;
        self.steps = self.steps.saturating_add(1);
        self.skip_before(at);
        // each entry stands by itself: none is held
        while self.listed.peek().is_some_and(|x| x < to) {
            self.steps = self.steps.saturating_add(1);
            let Some(e) = self.listed.take() else {
                break;
            };
            let block_end = e.offset.saturating_add(u64::from(e.len));
            if block_end > self.end {
                self.same = false;
            }
            self.damaged_end = self.damaged_end.max(block_end);
            self.damaged(&e);
        }
    }

    /// Entry `e` of a damaged block kept stands for it.
    fn damaged(&mut self, e: &IndexEntry) {
        if !e.repeat {
            match e.kind {
                BlockKind::Events | BlockKind::Series => {
                    if let Some(key) = self.key_of(e.track_number, Some(e.track_id)) {
                        self.order_of_damaged(e, key);
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

    /// Take the monotone relations of damaged chunk `e` of track `key`: the range relation by the range its entry
    /// states, the others unread, so the next chunk can't be checked against them.
    fn order_of_damaged(&mut self, e: &IndexEntry, key: Key) {
        if e.kind != BlockKind::Events {
            return;
        }
        if e.relation_break {
            self.restart(key);
        }
        let range = self.facts.get(&e.track_number).and_then(|f| f.range);
        self.tops.retain(|(k, r), _| *k != key || Some(*r) == range);
        if let (Some(relation), Some(values)) = (range, e.range) {
            self.monotone(e.offset, key, relation, values);
            if let Some((at, id, Some(r))) = self.order
                && at == e.offset
                && id == key.0
                && r == relation
            {
                self.order = Some((at, id, None));
            }
        }
    }

    /// Whether the piece's final index and its walk are the same set (every entry met, at a block walked or a damaged
    /// block in bytes no block explains, before the close), and each close row in `rows` states the hash its track's
    /// listed blocks fold to, incomplete exactly where the head or a damaged block leaves units out.
    pub(super) fn check(&self, rows: &[TrackRange]) -> bool {
        if !self.same || !self.listed.good() || self.listed.peek().is_some() {
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

    /// Entries in file order, from a list.
    struct InOrder(alloc::collections::VecDeque<IndexEntry>);

    impl Listed for InOrder {
        fn peek(&self) -> Option<u64> {
            self.0.front().map(|e| e.offset)
        }
        fn take(&mut self) -> Option<IndexEntry> {
            self.0.pop_front()
        }
        fn good(&self) -> bool {
            true
        }
    }

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
        let mut r = Replay::new(&[], InOrder(listed.into()), at + off);
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
