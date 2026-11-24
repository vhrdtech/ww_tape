//! State lookups: the record of a slot in force at a place, an item by its hash, every definition in force at a
//! place; see [state](crate::state).
//!
//! A lookup goes by the index's entries of state records ([StateEntry]), sorted by slot and by hash once per piece
//! and kept in the reader's cache, at most [STATE_CACHE_MAX] entries over all pieces (a setting), the least recently
//! used piece out first: the newest entry of the slot whose block starts before the place, then the next older
//! when that block doesn't read or its record isn't the one the entry names. What didn't read is listed ([Lost]), and
//! so are the stretches between the record and the place the index doesn't cover (gaps a scan read past, what lies
//! past a cut): a record there may be newer. Damage is never an error. A lookup reads each block once and spends at
//! most [STATE_READ_MAX] bytes (a setting), coded, decoded and held; it keeps only the records it may return. A coded
//! body's decoded length is charged before it is decoded. A lookup whose budget runs out stops there and says so
//! ([Lookup::spent]): that is not damage, and no block is listed lost for it.

use crate::TrackId;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use shrink_wrap::prelude::DeserializeShrinkWrap;

use super::{ByteSource, PieceInfo, R, ReadError, TapeReader, wide};
use crate::index::{IndexEntry, STATE_TRACK_ID, StateEntry};
use crate::piece::TrackStart;
use crate::state::{ItemHash, Slot};
use crate::{Block, ClockTime};

/// Most bytes one lookup spends by default ([OpenOptions::state_read_max](super::OpenOptions::state_read_max)): the
/// coded bytes it reads, the decoded bodies and the records it keeps.
pub const STATE_READ_MAX: exact_units::Bytes<u64> = exact_units::Bytes::new(1 << 30);

/// Most state entries a reader keeps sorted over all its pieces by default
/// ([OpenOptions::state_cache_max](super::OpenOptions::state_cache_max)): two pieces at the default cap
/// ([STATE_ENTRIES_MAX](crate::index::STATE_ENTRIES_MAX)).
///
/// A piece with more is sorted again for each lookup.
pub const STATE_CACHE_MAX: exact_units::Count<exact_units::units::Entry, u64> =
    exact_units::Count::new(1 << 21);

/// A reader's sorted state entries by piece, at most `max` entries over all pieces, the least recently used piece
/// out first.
#[derive(Debug)]
pub(super) struct StateCache {
    max: usize,
    #[cfg(feature = "std")]
    held: std::sync::Mutex<Cached>,
    #[cfg(not(feature = "std"))]
    held: core::cell::RefCell<Cached>,
}

/// What a [StateCache] holds.
#[derive(Debug, Default)]
struct Cached {
    /// Entries held over all pieces
    entries: usize,
    /// Counts lookups: a piece's last use
    uses: u64,
    /// By piece start: its last use and its sorted entries
    pieces: BTreeMap<u64, (u64, Arc<StateIndex>)>,
}

impl StateCache {
    pub(super) fn new(max: usize) -> Self {
        Self {
            max,
            held: Default::default(),
        }
    }

    #[cfg_attr(
        feature = "std",
        expect(
            clippy::expect_used,
            reason = "a poisoned lock panics: nothing that can panic runs while the cache is held"
        )
    )]
    fn with<T>(&self, f: impl FnOnce(&mut Cached) -> T) -> T {
        #[cfg(feature = "std")]
        let mut held = self
            .held
            .lock()
            .expect("nothing panics while the state cache is held");
        #[cfg(not(feature = "std"))]
        let mut held = self.held.borrow_mut();
        f(&mut held)
    }

    /// The sorted entries of the piece starting at `start`, if held: now its most recent use.
    fn get(&self, start: u64) -> Option<Arc<StateIndex>> {
        self.with(|c| {
            c.uses = c.uses.wrapping_add(1);
            let uses = c.uses;
            let (used, ix) = c.pieces.get_mut(&start)?;
            *used = uses;
            Some(Arc::clone(ix))
        })
    }

    /// Hold `ix` for the piece starting at `start`, the least recently used pieces out to make room; not held when
    /// it alone is over the budget. Another thread's entries for the piece, put meanwhile, are the same.
    fn put(&self, start: u64, ix: StateIndex) -> Arc<StateIndex> {
        let ix = Arc::new(ix);
        let n = ix.entries.len();
        if n > self.max {
            return ix;
        }
        self.with(|c| {
            c.uses = c.uses.wrapping_add(1);
            let uses = c.uses;
            if let Some((used, held)) = c.pieces.get_mut(&start) {
                *used = uses;
                return Arc::clone(held);
            }
            while c.entries.saturating_add(n) > self.max {
                let Some(oldest) = c
                    .pieces
                    .iter()
                    .min_by_key(|(_, (used, _))| *used)
                    .map(|(k, _)| *k)
                else {
                    break;
                };
                if let Some((_, gone)) = c.pieces.remove(&oldest) {
                    c.entries = c.entries.saturating_sub(gone.entries.len());
                }
            }
            c.entries = c.entries.saturating_add(n);
            c.pieces.insert(start, (uses, Arc::clone(&ix)));
            ix
        })
    }

    /// The entries held over all pieces, and the starts of the pieces held.
    #[cfg(test)]
    pub(super) fn held(&self) -> (usize, Vec<u64>) {
        self.with(|c| (c.entries, c.pieces.keys().copied().collect()))
    }
}

/// A state record as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateItem {
    pub slot: Slot,
    pub hash: ItemHash,
    /// Where its block starts in the piece.
    pub offset: u64,
    /// Its time, None on track 0.
    pub at: Option<ClockTime>,
    pub item: Vec<u8>,
}

/// A block of state records a lookup didn't take: it doesn't read, or its record isn't the one its entry names. The
/// slot may have a newer record than the one found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Lost {
    /// Where the block starts in the piece.
    pub offset: u64,
    pub slot: Slot,
}

/// What a lookup of one record found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lookup {
    /// The record, None when no entry led to one.
    pub item: Option<StateItem>,
    /// The newer blocks of the slot that didn't read.
    pub lost: Vec<Lost>,
    /// Stretches of the piece between the record (the piece's start when none was found) and the place that the
    /// index doesn't cover, as offsets in the piece: a newer record may lie there.
    pub uncovered: Vec<Range<u64>>,
    /// The lookup's budget ([OpenOptions::state_read_max](super::OpenOptions::state_read_max)) ran out before it found a record that reads: older candidates were
    /// not read.
    pub spent: bool,
}

impl Lookup {
    /// Whether nothing newer can hide: every block read, the budget lasted and the index covers the stretch to the
    /// place.
    pub fn is_whole(&self) -> bool {
        self.lost.is_empty() && self.uncovered.is_empty() && !self.spent
    }
}

/// Every definition in force at a place: each definition slot's newest record behind it, see
/// [TapeReader::definitions_at].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefinitionsAt {
    pub items: BTreeMap<Slot, StateItem>,
    /// Blocks that didn't read: the slots they held may keep an older definition, or none.
    pub lost: Vec<Lost>,
    /// Stretches of the piece before the place that the index doesn't cover, as offsets in the piece: a definition
    /// there is not among the items.
    pub uncovered: Vec<Range<u64>>,
    /// The lookup's budget ([OpenOptions::state_read_max](super::OpenOptions::state_read_max)) ran out: the slots not yet looked up are not among the items.
    pub spent: bool,
    /// The piece head's rows marked [TrackStart::not_carried] whose track has no definition among the items, under
    /// its number with its id and owner: the cut that started the piece had no room for it, so without the pieces
    /// before, the track is not defined. A marked row whose definition is among the items is not named.
    pub not_carried: Vec<TrackStart>,
}

impl DefinitionsAt {
    /// The definitions' items, by slot.
    pub fn items(&self) -> Vec<Vec<u8>> {
        self.items.values().map(|s| s.item.clone()).collect()
    }

    /// Whether every block read, the budget lasted and the index covers the piece to the place.
    pub fn is_whole(&self) -> bool {
        self.lost.is_empty() && self.uncovered.is_empty() && !self.spent
    }

    /// The number of the track of id `id` ([TrackDef::id](crate::TrackDef::id)) and owner `owner` among the
    /// definitions: how a reader finds a track it knows by its key.
    pub fn track_number(&self, id: TrackId, owner: [u8; 32]) -> Option<crate::TrackNum> {
        let tracks = self.items.iter().filter_map(|(slot, s)| match slot {
            Slot::Track { number } => Some((*number, s)),
            _ => None,
        });
        for (number, s) in tracks {
            if matches!(
                crate::Describe::from_ww_bytes(&s.item),
                Ok(crate::Describe::Track(t)) if t.id() == id && t.owner == owner
            ) {
                return Some(number);
            }
        }
        None
    }
}

/// Whether `items` hold the definition of head row `row`'s track: under its number, with its id and owner. A number
/// reused by another track doesn't count.
fn defines(items: &BTreeMap<Slot, StateItem>, row: &TrackStart) -> bool {
    let Some(item) = items.get(&Slot::Track {
        number: row.track_number,
    }) else {
        return false;
    };
    matches!(
        crate::Describe::from_ww_bytes(&item.item),
        Ok(crate::Describe::Track(t)) if t.id() == row.track_id && t.owner == row.owner.0
    )
}

/// The state entries of a piece, in file order and sorted: by slot then offset, and by hash then offset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct StateIndex {
    entries: Vec<(IndexEntry, StateEntry)>,
    by_slot: Vec<usize>,
    by_hash: Vec<usize>,
    /// The blocks of puts ([FLAG_REPEAT](crate::FLAG_REPEAT)) by offset: one entry of each
    puts: BTreeMap<u64, IndexEntry>,
}

impl StateIndex {
    fn new(entries: Vec<(IndexEntry, StateEntry)>) -> Self {
        let mut by_slot: Vec<usize> = (0..entries.len()).collect();
        let mut by_hash = by_slot.clone();
        let key = |i: &usize| entries.get(*i).map(|(e, s)| (s.slot, e.offset));
        by_slot.sort_by_key(key);
        by_hash.sort_by_key(|i| entries.get(*i).map(|(e, s)| (s.hash, e.offset)));
        let puts = entries
            .iter()
            .filter(|(e, _)| e.repeat)
            .map(|(e, _)| (e.offset, *e))
            .collect();
        Self {
            entries,
            by_slot,
            by_hash,
            puts,
        }
    }

    /// The block of puts that ends at `at`.
    fn puts_before(&self, at: u64) -> Option<&IndexEntry> {
        self.puts
            .range(..at)
            .next_back()
            .map(|(_, e)| e)
            .filter(|e| e.offset.saturating_add(u64::from(e.len)) == at)
    }

    /// The block of puts that starts at `at`.
    fn puts_at(&self, at: u64) -> Option<&IndexEntry> {
        self.puts.get(&at)
    }

    fn at(&self, i: &usize) -> Option<&(IndexEntry, StateEntry)> {
        self.entries.get(*i)
    }

    /// The entries of `slot` whose block starts before `before`, oldest first.
    fn of_slot(
        &self,
        slot: Slot,
        before: u64,
    ) -> impl DoubleEndedIterator<Item = &(IndexEntry, StateEntry)> {
        let key = |i: &usize| self.at(i).map(|(e, s)| (s.slot, e.offset));
        let lo = self.by_slot.partition_point(|i| key(i) < Some((slot, 0)));
        let hi = self
            .by_slot
            .partition_point(|i| key(i) < Some((slot, before)));
        self.by_slot
            .get(lo..hi.max(lo))
            .unwrap_or_default()
            .iter()
            .filter_map(|i| self.at(i))
    }

    /// The entries of item `hash`, oldest first.
    fn of_hash(
        &self,
        hash: ItemHash,
    ) -> impl DoubleEndedIterator<Item = &(IndexEntry, StateEntry)> {
        let key = |i: &usize| self.at(i).map(|(_, s)| s.hash);
        let lo = self.by_hash.partition_point(|i| key(i) < Some(hash));
        let hi = self.by_hash.partition_point(|i| key(i) <= Some(hash));
        self.by_hash
            .get(lo..hi.max(lo))
            .unwrap_or_default()
            .iter()
            .filter_map(|i| self.at(i))
    }

    /// The definition slots with an entry whose block starts before `before`.
    #[cfg(test)]
    fn definition_slots(&self, before: u64) -> Vec<Slot> {
        self.slots(before, Slot::is_definition)
    }

    /// The slots `of` takes with a record whose block starts before `before`, each once, in slot order.
    fn slots(&self, before: u64, of: impl Fn(&Slot) -> bool) -> Vec<Slot> {
        let mut out: Vec<Slot> = Vec::new();
        for (e, s) in self.by_slot.iter().filter_map(|i| self.at(i)) {
            if of(&s.slot) && e.offset < before && out.last() != Some(&s.slot) {
                out.push(s.slot);
            }
        }
        out
    }
}

/// The records of a block a lookup keeps: those it may return, as (slot, hash, time, item).
type Kept = Vec<(Slot, ItemHash, Option<ClockTime>, Vec<u8>)>;

/// What a lookup made of a block.
#[derive(Debug, PartialEq, Eq)]
enum Read {
    /// The block read: its header flags, the records the lookup keeps of it
    Kept { flags: u16, records: Kept },
    /// It didn't read, or isn't the block its entry names
    Bad,
    /// The budget has no room for it
    Spent,
}

/// What a lookup taking an entry found.
enum Taken {
    Item(StateItem),
    Lost,
    Spent,
}

/// What a lookup read: what it made of each block by offset.
struct Reads<'w> {
    blocks: BTreeMap<u64, Read>,
    /// The records the lookup may return, by slot and hash
    wanted: &'w BTreeSet<(Slot, ItemHash)>,
    /// What it may still spend
    left: u64,
}

impl Reads<'_> {
    /// Spend `n` bytes; false when they don't fit what is left.
    fn spend(&mut self, n: u64) -> bool {
        match self.left.checked_sub(n) {
            Some(left) => {
                self.left = left;
                true
            }
            None => false,
        }
    }
}

impl<S: ByteSource> TapeReader<S> {
    /// The index entries of piece `i`'s state records, in file order.
    pub fn state_entries(&self, i: usize) -> R<Vec<(IndexEntry, StateEntry)>, S> {
        Ok(self.state_index(i)?.entries.clone())
    }

    /// Piece `i`'s state entries sorted: from the reader's cache, or sorted and put there.
    fn state_index(&self, i: usize) -> R<Arc<StateIndex>, S> {
        let start = self.piece(i)?.start;
        if let Some(ix) = self.states.get(start) {
            return Ok(ix);
        }
        let entries: Vec<(IndexEntry, StateEntry)> = self
            .track_entries(i, STATE_TRACK_ID)?
            .into_iter()
            .filter_map(|e| Some((e, e.state()?)))
            .collect();
        Ok(self.states.put(start, StateIndex::new(entries)))
    }

    /// The record of `slot` in force at `offset` in piece `i`: the newest one whose block starts before `offset`.
    pub fn state_at(&self, i: usize, slot: Slot, offset: u64) -> R<Lookup, S> {
        let ix = self.state_index(i)?;
        let candidates: Vec<&(IndexEntry, StateEntry)> = ix.of_slot(slot, offset).rev().collect();
        let mut found = self.first_good(i, &ix, &candidates)?;
        let from = found.item.as_ref().map_or(0, |s| s.offset);
        found.uncovered = uncovered(self.piece(i)?, from..offset);
        Ok(found)
    }

    /// The hash of the record of `slot` in force at `offset` in piece `i`, from the index alone: the newest one whose
    /// block starts before `offset`.
    pub(super) fn hash_at(&self, i: usize, slot: Slot, offset: u64) -> R<Option<ItemHash>, S> {
        let ix = self.state_index(i)?;
        Ok(ix.of_slot(slot, offset).next_back().map(|(_, s)| s.hash))
    }

    /// The newest record of piece `i` whose item hashes to `hash`, of any slot. Its `uncovered` stretches run from
    /// the record to the piece's end.
    pub fn item_by_hash(&self, i: usize, hash: ItemHash) -> R<Lookup, S> {
        let ix = self.state_index(i)?;
        let candidates: Vec<&(IndexEntry, StateEntry)> = ix.of_hash(hash).rev().collect();
        let mut found = self.first_good(i, &ix, &candidates)?;
        let from = found.item.as_ref().map_or(0, |s| s.offset);
        found.uncovered = uncovered(self.piece(i)?, from..u64::MAX);
        Ok(found)
    }

    /// Every definition in force at `offset` in piece `i` (`u64::MAX`: at its end): each definition slot's newest
    /// record whose block starts before `offset`, an older one where that block doesn't read.
    pub fn definitions_at(&self, i: usize, offset: u64) -> R<DefinitionsAt, S> {
        self.definitions_of(i, offset, Slot::is_definition)
    }

    /// [Self::definitions_at] of only the definition slots `of` takes: a lookup that reads no other block.
    pub(crate) fn definitions_of(
        &self,
        i: usize,
        offset: u64,
        of: impl Fn(&Slot) -> bool,
    ) -> R<DefinitionsAt, S> {
        let ix = self.state_index(i)?;
        let slots = ix.slots(offset, |s| s.is_definition() && of(s));
        let wanted: BTreeSet<(Slot, ItemHash)> = slots
            .iter()
            .flat_map(|slot| ix.of_slot(*slot, offset).map(|(_, s)| (s.slot, s.hash)))
            .collect();
        let mut reads = Reads {
            blocks: BTreeMap::new(),
            wanted: &wanted,
            left: self.state_read_max,
        };
        let mut out = DefinitionsAt::default();
        'slots: for slot in slots {
            for (e, s) in ix.of_slot(slot, offset).rev() {
                match self.take(i, &ix, e, s, &mut reads)? {
                    Taken::Item(item) => {
                        out.items.insert(slot, item);
                        break;
                    }
                    Taken::Lost => out.lost.push(Lost {
                        offset: e.offset,
                        slot,
                    }),
                    Taken::Spent => {
                        out.spent = true;
                        break 'slots;
                    }
                }
            }
        }
        let piece = self.piece(i)?;
        out.uncovered = uncovered(piece, 0..offset);
        out.not_carried = piece
            .head
            .iter()
            .flat_map(|(_, rows)| rows)
            .filter(|r| r.not_carried && !defines(&out.items, r))
            .copied()
            .collect();
        Ok(out)
    }

    /// The newest record of every state track in piece `i` that reads, by track number: what a chain resumed after
    /// the piece goes on from. A slot whose records don't read, or past the budget, is left out.
    pub(crate) fn states_in(&self, i: usize) -> R<Vec<StateItem>, S> {
        let ix = self.state_index(i)?;
        let slots = ix.slots(u64::MAX, |s| matches!(s, Slot::State { .. }));
        let mut out = Vec::new();
        for slot in slots {
            let candidates: Vec<&(IndexEntry, StateEntry)> =
                ix.of_slot(slot, u64::MAX).rev().collect();
            if let Some(item) = self.first_good(i, &ix, &candidates)?.item {
                out.push(item);
            }
        }
        Ok(out)
    }

    /// The first of `candidates` (newest first) whose record reads and is the one its entry names.
    fn first_good(
        &self,
        i: usize,
        ix: &StateIndex,
        candidates: &[&(IndexEntry, StateEntry)],
    ) -> R<Lookup, S> {
        let wanted: BTreeSet<(Slot, ItemHash)> =
            candidates.iter().map(|(_, s)| (s.slot, s.hash)).collect();
        let mut reads = Reads {
            blocks: BTreeMap::new(),
            wanted: &wanted,
            left: self.state_read_max,
        };
        let mut found = Lookup::default();
        for (e, s) in candidates {
            match self.take(i, ix, e, s, &mut reads)? {
                Taken::Item(item) => {
                    found.item = Some(item);
                    break;
                }
                Taken::Lost => found.lost.push(Lost {
                    offset: e.offset,
                    slot: s.slot,
                }),
                Taken::Spent => {
                    found.spent = true;
                    break;
                }
            }
        }
        Ok(found)
    }

    /// The record entry `e` (listing `s`) names, its block read once into `reads`: the first of the block's records
    /// of the entry's slot and hash. Lost when the block doesn't read or holds no such record, and when a block of
    /// its group doesn't ([Self::group_reads]).
    fn take(
        &self,
        i: usize,
        ix: &StateIndex,
        e: &IndexEntry,
        s: &StateEntry,
        reads: &mut Reads<'_>,
    ) -> R<Taken, S> {
        let kept = match self.read_once(i, e, reads)? {
            Read::Kept { records, .. } => records,
            Read::Spent => return Ok(Taken::Spent),
            Read::Bad => return Ok(Taken::Lost),
        };
        let Some((_, _, at, item)) = kept
            .iter()
            .find(|(slot, hash, ..)| *hash == s.hash && *slot == s.slot)
        else {
            return Ok(Taken::Lost);
        };
        let item = StateItem {
            slot: s.slot,
            hash: s.hash,
            offset: e.offset,
            at: *at,
            item: item.clone(),
        };
        Ok(match self.group_reads(i, ix, e, reads)? {
            Read::Kept { .. } => Taken::Item(item),
            Read::Spent => Taken::Spent,
            Read::Bad => Taken::Lost,
        })
    }

    /// What `reads` made of the block entry `e` names in piece `i`, read now when it hasn't been.
    fn read_once<'r>(&self, i: usize, e: &IndexEntry, reads: &'r mut Reads<'_>) -> R<&'r Read, S> {
        if !reads.blocks.contains_key(&e.offset) {
            let read = self.kept_of(i, e, reads)?;
            reads.blocks.insert(e.offset, read);
        }
        Ok(reads.blocks.get(&e.offset).unwrap_or(&Read::Bad))
    }

    /// Whether every block of the group of puts ([FLAG_GROUP](crate::FLAG_GROUP)) the block entry `e` names belongs
    /// to reads, each read once into `reads`: [Read::Bad] when one doesn't, [Read::Spent] when the budget runs out
    /// first, else an empty [Read::Kept]. A block of no group stands alone.
    ///
    /// The group is the block, the blocks of the group before it back to its first one
    /// ([FLAG_GROUP_START](crate::FLAG_GROUP_START)), and when the block is marked
    /// [FLAG_WITH_NEXT](crate::FLAG_WITH_NEXT), the blocks after it up to the first unmarked one. A block of puts
    /// that doesn't read hides its marks, so before a block of a group without a start it may be of the group: Bad,
    /// as a scan drops such blocks after damage. So is a group whose first block is missing. A group of more than [RUN_BLOCKS_MAX](crate::RUN_BLOCKS_MAX) blocks
    /// is no group a writer makes: Bad.
    fn group_reads(
        &self,
        i: usize,
        ix: &StateIndex,
        e: &IndexEntry,
        reads: &mut Reads<'_>,
    ) -> R<Read, S> {
        let whole = Read::Kept {
            flags: 0,
            records: Vec::new(),
        };
        // (of a group, marked FLAG_WITH_NEXT, the group's first)
        let marks = |r: &Read| match r {
            Read::Kept { flags, .. } => Some((
                flags & crate::FLAG_GROUP != 0,
                flags & crate::FLAG_WITH_NEXT != 0,
                flags & crate::FLAG_GROUP_START != 0,
            )),
            Read::Bad | Read::Spent => None,
        };
        let (group, mut marked, mut start) = match self.read_once(i, e, reads)? {
            Read::Spent => return Ok(Read::Spent),
            r => marks(r).unwrap_or((false, false, false)),
        };
        if !group {
            return Ok(whole);
        }
        let mut steps = 0usize;
        // backwards: the marked blocks of the group the block goes on, back to its first
        let mut at = e.offset;
        while !start {
            let Some(prev) = ix.puts_before(at) else {
                return Ok(Read::Bad);
            };
            steps = steps.saturating_add(1);
            if steps > crate::RUN_BLOCKS_MAX {
                return Ok(Read::Bad);
            }
            match self.read_once(i, prev, reads)? {
                Read::Spent => return Ok(Read::Spent),
                r => match marks(r) {
                    Some((true, true, first)) => {
                        at = prev.offset;
                        start = first;
                    }
                    _ => return Ok(Read::Bad),
                },
            }
        }
        // forwards: the blocks after a marked one up to the first unmarked one
        let mut end = e.offset.saturating_add(u64::from(e.len));
        while marked {
            let Some(next) = ix.puts_at(end) else {
                return Ok(whole);
            };
            steps = steps.saturating_add(1);
            if steps > crate::RUN_BLOCKS_MAX {
                return Ok(Read::Bad);
            }
            match self.read_once(i, next, reads)? {
                Read::Spent => return Ok(Read::Spent),
                Read::Bad => return Ok(Read::Bad),
                r @ Read::Kept { .. } => match marks(r) {
                    Some((true, m, false)) => marked = m,
                    _ => return Ok(Read::Bad),
                },
            }
            end = next.offset.saturating_add(u64::from(next.len));
        }
        Ok(whole)
    }

    /// The records of the record block `e` names in piece `i` that `reads` wants, the bytes read, decoded and kept
    /// spent from its budget. A source that fails is an error.
    fn kept_of(&self, i: usize, e: &IndexEntry, reads: &mut Reads<'_>) -> R<Read, S> {
        if !reads.spend(u64::from(e.len)) {
            return Ok(Read::Spent);
        }
        let mut buf = Vec::new();
        let block = match self.read_block(i, e, &mut buf) {
            Ok(b) => b,
            Err(ReadError::Source(err)) => return Err(ReadError::Source(err)),
            Err(_) => return Ok(Read::Bad),
        };
        Ok(kept(&block, e, reads))
    }
}

/// The records of `block` that `reads` wants, read where `e` says. A coded body's decoded length, from the body's
/// head, is spent before it is decoded.
fn kept(block: &Block<'_>, e: &IndexEntry, reads: &mut Reads<'_>) -> Read {
    if block.kind != e.kind || wide(block.len()) != u64::from(e.len) {
        return Read::Bad;
    }
    if let Some(len) = block.decoded_len()
        && !reads.spend(wide(len))
    {
        return Read::Spent;
    }
    let mut buf = Vec::new();
    let Ok(b) = block.decode(&mut buf) else {
        return Read::Bad;
    };
    let mut out = Vec::new();
    for r in b.records() {
        let Ok(r) = r else {
            return Read::Bad;
        };
        let Some(slot) = Slot::of(r.track, r.item) else {
            continue;
        };
        let hash = ItemHash::of(r.item);
        if reads.wanted.contains(&(slot, hash)) {
            if !reads.spend(wide(r.item.len())) {
                return Read::Spent;
            }
            out.push((slot, hash, r.at, r.item.to_vec()));
        }
    }
    Read::Kept {
        flags: block.flags,
        records: out,
    }
}

/// The stretches of `stretch` (offsets in piece `p`) that `p`'s index doesn't cover: the gaps a scan read past,
/// what lies past a cut, and a place past the piece's end (`u64::MAX` stands for its end).
fn uncovered(p: &PieceInfo, stretch: Range<u64>) -> Vec<Range<u64>> {
    let mut out: Vec<Range<u64>> = p
        .gaps
        .iter()
        .map(|(at, len)| {
            let from = at.saturating_sub(p.start);
            from..from.saturating_add(*len)
        })
        .collect();
    if let Some(cut) = p.cut {
        out.push(cut.saturating_sub(p.start)..u64::MAX);
    }
    if stretch.end != u64::MAX {
        out.push(p.end.saturating_sub(p.start)..u64::MAX);
    }
    out.into_iter()
        .filter_map(|r| {
            let from = r.start.max(stretch.start);
            let to = r.end.min(stretch.end);
            (from < to).then_some(from..to)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::code_body;
    use crate::open::{RecordRef, write_records};
    use crate::reader::Found;
    use crate::{BlockId, BodyCodec, SessionTag};
    use crate::{ClockNum, RecordTrack, TrackNum};

    /// An lz4-coded block of `n` state records on track 5, `len` bytes each, the `i`th all `i`.
    fn coded(n: u8, len: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let items: Vec<Vec<u8>> = (0..n).map(|i| alloc::vec![i; len]).collect();
        let records: Vec<RecordRef<'_>> = items
            .iter()
            .enumerate()
            .map(|(i, item)| RecordRef {
                track: RecordTrack::from_u32(5),
                at: Some(ClockTime::new(ClockNum::new(1), i64::try_from(i).unwrap())),
                item,
            })
            .collect();
        let mut plain = Vec::new();
        write_records(
            &records,
            1 << 20,
            0,
            || BlockId::new(SessionTag::from_u32(7), 0),
            |b| {
                plain = b.to_vec();
                Ok::<(), crate::DescribeError>(())
            },
        )
        .unwrap();
        let mut out = alloc::vec![0; crate::coded_len_max(plain.len())];
        let len = code_body(&plain, BodyCodec::LZ4, &mut out).unwrap();
        out.truncate(len);
        (out, items)
    }

    fn reads(wanted: &BTreeSet<(Slot, ItemHash)>, left: u64) -> Reads<'_> {
        Reads {
            blocks: BTreeMap::new(),
            wanted,
            left,
        }
    }

    /// A coded block costs its decoded bytes and the records kept, not its coded length: a budget that covers the
    /// coded bytes only takes nothing (reading side review 2, point 5).
    #[test]
    fn decoded_and_kept_bytes_are_spent() {
        let (block, items) = coded(8, 4000);
        let b = Block::read(&block).unwrap();
        assert!(b.is_coded());
        let e = IndexEntry::of_block(0, &b).unwrap();
        let slot = Slot::State {
            track: TrackNum::new(5),
        };
        let wanted: BTreeSet<_> = items.iter().map(|i| (slot, ItemHash::of(i))).collect();
        let mut r = reads(&wanted, wide(block.len()));
        assert_eq!(kept(&b, &e, &mut r), Read::Spent);
        let decoded = 8 * 4000 + 200;
        let mut r = reads(&wanted, decoded);
        assert_eq!(
            kept(&b, &e, &mut r),
            Read::Spent,
            "the records kept are spent too"
        );
        let mut r = reads(&wanted, 2 * decoded);
        assert!(matches!(kept(&b, &e, &mut r), Read::Kept { records: k, .. } if k.len() == 8));
    }

    /// A coded body's decoded length is charged before it is decoded: a damaged body over the budget is the budget's
    /// answer, never decoded (reading side review 3, point 4).
    #[test]
    fn the_decoded_length_is_charged_first() {
        let (mut block, items) = coded(8, 4000);
        let n = block.len();
        // the lz4 sequences garbled, the head that states the decoded length kept
        block[n - 40..n - 4].fill(0xFF);
        let crc = crate::CRC.checksum(&block[..n - 4]);
        block[n - 4..].copy_from_slice(&crc.to_le_bytes());
        let b = Block::read(&block).unwrap();
        let e = IndexEntry::of_block(0, &b).unwrap();
        let slot = Slot::State {
            track: TrackNum::new(5),
        };
        let wanted: BTreeSet<_> = items.iter().map(|i| (slot, ItemHash::of(i))).collect();
        let left = 8 * 4000;
        let mut r = reads(&wanted, left);
        assert_eq!(kept(&b, &e, &mut r), Read::Spent);
        assert_eq!(r.left, left, "nothing spent on a body that doesn't fit");
        let mut r = reads(&wanted, u64::MAX);
        assert_eq!(
            kept(&b, &e, &mut r),
            Read::Bad,
            "with room it decodes and is damaged"
        );
    }

    /// A lookup keeps only the records it may return: one wanted record of eight costs its own bytes alone.
    #[test]
    fn only_the_records_wanted_are_kept() {
        let (block, items) = coded(8, 4000);
        let b = Block::read(&block).unwrap();
        let e = IndexEntry::of_block(0, &b).unwrap();
        let slot = Slot::State {
            track: TrackNum::new(5),
        };
        let wanted = BTreeSet::from([(slot, ItemHash::of(&items[3]))]);
        let mut r = reads(&wanted, u64::MAX);
        let Read::Kept { records: k, .. } = kept(&b, &e, &mut r) else {
            panic!("the block reads");
        };
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].3, items[3]);
        let mut buf = Vec::new();
        let decoded = wide(b.decode(&mut buf).unwrap().len());
        assert_eq!(u64::MAX - r.left, decoded + 4000);
    }

    /// The stretches the index doesn't cover: the gaps, what lies past a cut, a place past the end.
    #[test]
    fn uncovered_stretches() {
        let mut p = PieceInfo::new(100, Found::Scan { scanned: 0 });
        p.end = 1100;
        p.gaps = alloc::vec![(300, 50)];
        assert_eq!(uncovered(&p, 0..u64::MAX), alloc::vec![200..250; 1]);
        assert_eq!(uncovered(&p, 250..u64::MAX), []);
        assert_eq!(uncovered(&p, 0..2000), [200..250, 1000..2000]);
        assert_eq!(uncovered(&p, 0..900), alloc::vec![200..250; 1]);
        p.cut = Some(900);
        assert_eq!(uncovered(&p, 0..u64::MAX), [200..250, 800..u64::MAX]);
        assert_eq!(uncovered(&p, 0..850), [200..250, 800..850]);
    }

    /// An entry of a state record of `slot` holding `item` in a block at `offset`.
    fn entry(offset: u64, slot: Slot, item: &[u8]) -> (IndexEntry, StateEntry) {
        let e = IndexEntry {
            offset,
            len: 0,
            kind: crate::BlockKind::Records,
            level: 0,
            tree: 0,
            track_id: STATE_TRACK_ID,
            number: 0,
            second: 0,
            first_item: crate::ItemNum::new(0),
            count: 0,
            first_tick: 0,
            last_tick: 0,
            latest_end: 0,
            repeat: false,
            relation_break: false,
            unit: crate::piece::UnitHash::NONE,
            unhashed: false,
            range: None,
            tile: None,
        };
        (
            e,
            StateEntry {
                slot,
                hash: ItemHash::of(item),
                ticks: 0,
            },
        )
    }

    /// A reader holds at most its budget of sorted entries over all pieces, the least recently used out first; a
    /// piece over the budget alone is not held (reading side review 3, point 5).
    #[test]
    fn the_state_cache_keeps_a_budget_over_all_pieces() {
        let index = |n: u64| {
            let slot = Slot::State {
                track: TrackNum::new(5),
            };
            StateIndex::new((0..n).map(|k| entry(k, slot, b"x")).collect())
        };
        let cache = StateCache::new(10);
        cache.put(100, index(4));
        cache.put(200, index(4));
        assert_eq!(cache.held(), (8, alloc::vec![100, 200]));
        // piece 100 used: piece 200 is the least recently used
        assert!(cache.get(100).is_some());
        cache.put(300, index(4));
        assert_eq!(cache.held(), (8, alloc::vec![100, 300]));
        assert!(cache.get(200).is_none());
        // one piece over the budget alone: returned, not held
        assert_eq!(cache.put(400, index(11)).entries.len(), 11);
        assert_eq!(cache.held(), (8, alloc::vec![100, 300]));
        // a piece that takes the whole budget leaves nothing else
        cache.put(500, index(10));
        assert_eq!(cache.held(), (10, alloc::vec![500]));
    }

    /// The sorted entries find a slot's entries before a place and an item's entries by binary search.
    #[test]
    fn the_sorted_index_finds_slots_and_hashes() {
        let (a, b) = (
            Slot::State {
                track: TrackNum::new(5),
            },
            Slot::Track {
                number: TrackNum::new(2),
            },
        );
        let ix = StateIndex::new(alloc::vec![
            entry(10, a, b"x"),
            entry(20, b, b"t"),
            entry(30, a, b"y"),
            entry(40, a, b"x"),
        ]);
        let offsets = |it: &mut dyn Iterator<Item = &(IndexEntry, StateEntry)>| {
            it.map(|(e, _)| e.offset).collect::<Vec<_>>()
        };
        assert_eq!(offsets(&mut ix.of_slot(a, 40)), [10, 30]);
        assert_eq!(offsets(&mut ix.of_slot(a, u64::MAX)), [10, 30, 40]);
        assert_eq!(offsets(&mut ix.of_slot(b, 20)), Vec::<u64>::new());
        assert_eq!(offsets(&mut ix.of_hash(ItemHash::of(b"x"))), [10, 40]);
        assert_eq!(ix.definition_slots(u64::MAX), [b]);
        assert_eq!(ix.definition_slots(20), Vec::<Slot>::new());
    }
}
