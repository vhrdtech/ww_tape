//! [Indexer]: what a writer keeps to place checkpoints and write the final index and the close.

use crate::ItemNum;

use crate::TrackId;
use crate::TrackNum;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use super::{
    CHECKPOINT_ENTRIES_MAX, ENTRY_LEN, IndexEntry, IndexError, IndexHead, IndexKind, IndexPolicy,
    STATE_TRACK_ID, index_len, track_count, write_index,
};
use crate::bytes::le_u8;
use crate::chunk::len_u64;
use crate::piece::{OwnerId, TrackHash, TrackRange, TrackStart, UnitHash};
use crate::{Block, BlockKind, Chunk, HEADER_LEN, MAX_BLOCK_LEN, VERSION};

/// Most entries in one final index block.
const FINAL_ENTRIES_MAX: usize = (MAX_BLOCK_LEN >> 1) / ENTRY_LEN;

/// A track in a piece: its id and owner.
pub(crate) type TrackIdent = (TrackId, OwnerId);

/// A track's hash and counted records in a piece.
#[derive(Copy, Clone, Debug)]
struct Tally {
    number: TrackNum,
    hash: TrackHash,
    records: u64,
    /// The piece holds a counted chunk or record of the track: the close lists it
    touched: bool,
    /// The hash leaves out units a resume kept unread ([TrackRange::incomplete])
    incomplete: bool,
}

/// The blocks of a piece as they are placed: entries not yet in a checkpoint, every entry for the final index,
/// the item and tick range of each track's chunks, and each track's hash.
#[derive(Debug, Default)]
pub(crate) struct Indexer {
    policy: IndexPolicy,
    pending: Vec<IndexEntry>,
    all: Vec<IndexEntry>,
    /// Entries of state records among `all`
    states: usize,
    /// Bytes placed since the last checkpoint
    since: u64,
    /// The index block the next checkpoint points back at: the newest checkpoint, or the last close's final index
    chained: Option<u64>,
    /// The newest checkpoint of the piece, before a close it went on after too
    last_checkpoint: Option<u64>,
    tracks: BTreeMap<TrackIdent, TrackRange>,
    /// The track each number stands for, from the definitions placed in the piece
    numbers: BTreeMap<TrackNum, TrackIdent>,
    /// The tracks the piece head carries over, by number: where a number has no definition placed (a damaged one)
    carried: BTreeMap<TrackNum, TrackIdent>,
    tallies: BTreeMap<TrackIdent, Tally>,
    /// The tallies before the last counted block of data, to take it back ([Self::undo_block])
    undo: Vec<(TrackIdent, Option<Tally>)>,
    /// The tracks with a row in the close: in `tracks`, or tallied by a counted block or a state record
    /// ([Self::has_row])
    rows: BTreeSet<TrackIdent>,
    /// The tracks a state record of is placed in the piece ([Self::state_placed])
    stated: BTreeMap<TrackIdent, TrackNum>,
    /// The tracks the piece head carries over
    head_keys: BTreeSet<TrackIdent>,
    /// Those of `rows` the head doesn't carry
    rows_past_head: usize,
    /// The close record the writer went on after: the final index lists only the entries past it, chained back to
    /// that close's final index
    closed: Option<(u64, Option<u64>)>,
    /// The piece is below [Integrity::Full](crate::Integrity::Full): its entries are marked unhashed, no unit goes
    /// into a track's hash, and each close row's hash is absent (zero, marked incomplete)
    unhashed: bool,
}

impl Indexer {
    /// Hash the tracks and mark the entries hashed (`hashed`), or neither: a piece below
    /// [Integrity::Full](crate::Integrity::Full).
    pub(crate) fn set_hashed(&mut self, hashed: bool) {
        self.unhashed = !hashed;
    }

    pub(crate) fn new(policy: IndexPolicy) -> Self {
        Self {
            policy,
            ..Default::default()
        }
    }

    pub(crate) fn policy(&self) -> IndexPolicy {
        self.policy
    }

    pub(crate) fn set_policy(&mut self, policy: IndexPolicy) {
        self.policy = policy;
    }

    /// Start from the tracks the piece head carries over: their hashes go on.
    pub(crate) fn start(&mut self, starts: &[TrackStart]) {
        for t in starts {
            let key = (t.track_id, t.owner);
            self.carried.insert(t.track_number, key);
            self.head_keys.insert(key);
            self.tallies.insert(
                key,
                Tally {
                    number: t.track_number,
                    hash: t.hash,
                    records: 0,
                    touched: false,
                    incomplete: t.incomplete,
                },
            );
        }
    }

    /// Go on from the close record whose rows are `rows`, after [Self::start] from the piece head: each track's range,
    /// hash and counted records as the close lists them, its number standing for it as its definition placed before
    /// did. A resume of a closed piece takes them instead of replaying the blocks before the close.
    pub(crate) fn seed_close(&mut self, rows: &[TrackRange]) {
        for r in rows {
            let key = (r.track_id, r.owner);
            self.numbers.insert(r.track_number, key);
            if r.chunks > 0 {
                self.tracks.insert(key, *r);
            }
            self.tallies.insert(
                key,
                Tally {
                    number: r.track_number,
                    hash: r.hash,
                    records: r.records,
                    touched: true,
                    incomplete: r.incomplete,
                },
            );
            self.insert_row(key);
        }
    }

    /// Track number `number` stands for track `id` of `owner` from here on: a track definition was placed.
    pub(crate) fn define(&mut self, number: TrackNum, id: TrackId, owner: OwnerId) {
        self.numbers.insert(number, (id, owner));
    }

    /// The track of number `number`, its id `id` when the block says it: by the definition placed in the piece, else
    /// by the head; a chunk whose number names no track of its id is of the piece's owner.
    pub(crate) fn key_of(&self, number: TrackNum, id: Option<TrackId>) -> Option<TrackIdent> {
        let fits = |k: &&TrackIdent| id.is_none_or(|id| k.0 == id);
        self.numbers
            .get(&number)
            .filter(fits)
            .or_else(|| self.carried.get(&number).filter(fits))
            .copied()
            .or_else(|| id.map(|id| (id, OwnerId::NONE)))
    }

    /// A counted block of data comes: what it adds to the hashes can be taken back until the next one.
    pub(crate) fn begin_block(&mut self) {
        self.undo.clear();
    }

    /// Take back what the last counted block of data added to the hashes: a block with its id and other bytes
    /// replaces it.
    pub(crate) fn undo_block(&mut self) {
        let undo = core::mem::take(&mut self.undo);
        for (key, before) in undo.into_iter().rev() {
            match before {
                Some(t) => self.tallies.insert(key, t),
                None => self.tallies.remove(&key),
            };
            if let Some(number) = self.stated.get(&key).copied() {
                self.state_placed(key, number);
            }
            self.note_row(key);
        }
    }

    fn tally(
        &mut self,
        key: TrackIdent,
        number: TrackNum,
        unit: impl FnOnce(&TrackHash) -> TrackHash,
        records: u64,
    ) {
        let before = self.tallies.get(&key).copied();
        self.undo.push((key, before));
        let mut t = before.unwrap_or(Tally {
            number,
            hash: TrackHash::START,
            records: 0,
            touched: false,
            incomplete: false,
        });
        t.number = number;
        if !self.unhashed {
            t.hash = unit(&t.hash);
        }
        t.records = t.records.saturating_add(records);
        t.touched = true;
        self.tallies.insert(key, t);
        self.insert_row(key);
    }

    /// Keep `rows` as `key`'s chunks and tally say, after either changed.
    fn note_row(&mut self, key: TrackIdent) {
        if self.tracks.contains_key(&key) || self.tallies.get(&key).is_some_and(|t| t.touched) {
            self.insert_row(key);
        } else if self.rows.remove(&key) && !self.head_keys.contains(&key) {
            self.rows_past_head = self.rows_past_head.saturating_sub(1);
        }
    }

    /// Give track `key` a row in the close.
    fn insert_row(&mut self, key: TrackIdent) {
        if self.rows.insert(key) && !self.head_keys.contains(&key) {
            self.rows_past_head = self.rows_past_head.saturating_add(1);
        }
    }

    /// A state record of track `key` (number `number`) is placed: the track gets a row in the close, so the next
    /// head carries it, its hash and counted records unchanged.
    pub(crate) fn state_placed(&mut self, key: TrackIdent, number: TrackNum) {
        let t = self.tallies.entry(key).or_insert(Tally {
            number,
            hash: TrackHash::START,
            records: 0,
            touched: false,
            incomplete: false,
        });
        t.number = number;
        t.touched = true;
        self.stated.insert(key, number);
        self.insert_row(key);
    }

    /// The counted chunk of track `number` of id `id` with unit hash `unit` goes into its track's hash: one read,
    /// or a damaged one by the unit hash its index entry states.
    pub(crate) fn tally_chunk(&mut self, number: TrackNum, id: TrackId, unit: &UnitHash) {
        if let Some(key) = self.key_of(number, Some(id)) {
            self.tally(key, number, |h| h.then(unit), 0);
        }
    }

    /// Records of data a resume keeps unread, in a block whose entry doesn't say which tracks they are on: every
    /// track the piece defines or its head carries for which `takes` holds is marked incomplete.
    pub(crate) fn tally_unread_records(&mut self, takes: impl Fn(TrackNum) -> bool) {
        let tracks: BTreeMap<TrackIdent, TrackNum> = self
            .carried
            .iter()
            .chain(&self.numbers)
            .filter(|(n, _)| takes(**n))
            .map(|(n, k)| (*k, *n))
            .collect();
        for (key, number) in tracks {
            self.tally(key, number, |h| *h, 0);
            if let Some(t) = self.tallies.get_mut(&key) {
                t.incomplete = true;
            }
        }
    }

    /// Units of track `number` of id `id` that a resume keeps unread (a damaged block of its data whose entry holds no
    /// unit hash): its hash leaves them out, and the close marks it incomplete.
    pub(crate) fn tally_unread(&mut self, number: TrackNum, id: TrackId) {
        if let Some(key) = self.key_of(number, Some(id)) {
            self.tally(key, number, |h| *h, 0);
            if let Some(t) = self.tallies.get_mut(&key) {
                t.incomplete = true;
            }
        }
    }

    /// The counted record on track `number` at `ticks` holding `item` goes into its track's hash; a number no
    /// definition in the piece or the head names is left out.
    pub(crate) fn tally_record(&mut self, number: TrackNum, ticks: i64, item: &[u8]) {
        if let Some(key) = self.key_of(number, None) {
            self.tally(key, number, |h| h.record(ticks, item), 1);
        }
    }

    /// Note the block `bytes` (exactly one block, written by this writer) placed at `offset` in the piece, a chunk
    /// with what its entry carries beside its head (`extras`). Its CRC is not checked again: a chunk block's head is
    /// read by its head CRC.
    pub(crate) fn placed(&mut self, offset: u64, bytes: &[u8], extras: ChunkExtras) {
        let mut entries = of_placed(offset, bytes, extras);
        if self.unhashed {
            for e in &mut entries {
                e.unhashed = true;
            }
        }
        self.placed_entries(&entries, bytes.len());
    }

    /// Note a `len`-byte block placed whose entries are `entries` (none: a block no index lists; one per state record
    /// of a block of them): also a block a resume finds damaged, listed by the index read back.
    pub(crate) fn placed_entries(&mut self, entries: &[IndexEntry], len: usize) {
        self.placed_unlisted(len);
        for e in entries {
            if matches!(e.kind, BlockKind::Events | BlockKind::Series)
                && let Some(track) = e.track()
                && let Some(key) = self.key_of(track, Some(e.track_id))
            {
                add_to_range(&mut self.tracks, key, track, e);
                self.insert_row(key);
            }
            if e.track_id == STATE_TRACK_ID {
                self.states = self.states.saturating_add(1);
            }
            self.pending.push(*e);
            self.all.push(*e);
        }
    }

    /// Entries of state records the piece lists.
    pub(crate) fn states(&self) -> usize {
        self.states
    }

    /// Refuse to list `n` more state entries past the policy's cap.
    pub(crate) fn room_for_states(&self, n: usize) -> Result<(), IndexError> {
        let max = crate::cap_usize(self.policy.state_entries_max.get());
        if self.states.saturating_add(n) > max {
            return Err(IndexError::StateEntries {
                max,
                used: self.states,
            });
        }
        Ok(())
    }

    /// Every entry of the piece so far, in the order placed.
    pub(crate) fn entries(&self) -> &[IndexEntry] {
        &self.all
    }

    /// Note `len` bytes placed that no index lists: a block written again with the same bytes, which readers drop.
    pub(crate) fn placed_unlisted(&mut self, len: usize) {
        // past u64::MAX bytes a checkpoint is due all the same
        self.since = self.since.saturating_add(len_u64(len));
    }

    /// Drop the entries of the block at `offset`: a later block with its id and other bytes takes its place, as
    /// readers take it. A checkpoint placed already keeps them; the final index doesn't.
    pub(crate) fn replaced(&mut self, offset: u64) {
        let Some(gone) = self.all.iter().rfind(|e| e.offset == offset).copied() else {
            return;
        };
        let states = self
            .all
            .iter()
            .filter(|e| e.offset == offset && e.track_id == STATE_TRACK_ID)
            .count();
        self.states = self.states.saturating_sub(states);
        self.all.retain(|e| e.offset != offset);
        self.pending.retain(|e| e.offset != offset);
        if matches!(gone.kind, BlockKind::Events | BlockKind::Series)
            && let Some(track) = gone.track()
            && let Some(key) = self.key_of(track, Some(gone.track_id))
        {
            self.tracks.remove(&key);
            let same: Vec<IndexEntry> = self
                .all
                .iter()
                .filter(|e| {
                    matches!(e.kind, BlockKind::Events | BlockKind::Series)
                        && e.track().and_then(|t| self.key_of(t, Some(e.track_id))) == Some(key)
                })
                .copied()
                .collect();
            for e in &same {
                add_to_range(&mut self.tracks, key, track, e);
            }
            self.note_row(key);
        }
    }

    /// Entries not yet listed by a checkpoint.
    pub(crate) fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Bytes of the checkpoint blocks listing the pending entries ([Self::checkpoint] until none is left).
    pub(crate) fn checkpoint_bytes(&self) -> usize {
        let mut left = self.pending.len();
        let mut n = 0usize;
        while left > 0 {
            let listed = left.min(CHECKPOINT_ENTRIES_MAX);
            n = n.saturating_add(index_len(listed, 0));
            left = left.saturating_sub(listed);
        }
        n
    }

    /// Whether the policy's byte count, or the entry limit, asks for a checkpoint now.
    pub(crate) fn due(&self) -> bool {
        !self.pending.is_empty()
            && ((self.policy.bytes.get() > 0 && self.since >= self.policy.bytes.get())
                || self.pending.len() >= CHECKPOINT_ENTRIES_MAX)
    }

    /// The checkpoint index block listing the pending entries, at most [CHECKPOINT_ENTRIES_MAX] of them (the rest
    /// wait for the next one), to be placed at `offset` as block `id`.
    pub(crate) fn checkpoint(
        &mut self,
        offset: u64,
        id: crate::BlockId,
    ) -> Result<Vec<u8>, IndexError> {
        let head = IndexHead {
            kind: IndexKind::Checkpoint,
            self_offset: offset,
            prev: self.chained,
        };
        let n = self.pending.len().min(CHECKPOINT_ENTRIES_MAX);
        let listed = self.pending.get(..n).unwrap_or_default();
        let mut buf = vec![0u8; index_len(listed.len(), 0)];
        write_index(&mut buf, id, &head, listed)?;
        self.pending.drain(..n);
        self.since = 0;
        self.chained = Some(offset);
        self.last_checkpoint = Some(offset);
        Ok(buf)
    }

    /// The newest checkpoint's offset.
    pub(crate) fn last_checkpoint(&self) -> Option<u64> {
        self.last_checkpoint
    }

    /// The piece was closed by the close record at `offset`, its last final index block at `final_index` and its
    /// newest checkpoint at `checkpoint`: what is placed from here on is listed by checkpoints chained back to that
    /// final index, and by a final index of its own whose first block points at it. The tracks' ranges and hashes go
    /// on over the whole piece.
    pub(crate) fn closed(
        &mut self,
        offset: u64,
        final_index: Option<u64>,
        checkpoint: Option<u64>,
    ) {
        self.closed = Some((offset, final_index));
        self.pending.clear();
        self.since = 0;
        self.chained = final_index;
        self.last_checkpoint = checkpoint;
    }

    /// The final index block the next close's final index points back at: the last one of the close the writer went
    /// on after.
    pub(crate) fn final_before(&self) -> Option<u64> {
        self.closed.and_then(|(_, f)| f)
    }

    /// Every entry since the last close (all of them before the first), sorted for the final index, cut into the
    /// parts one final block each holds: whole tracks while they fit, a track longer than a block over several.
    pub(crate) fn final_parts(&self) -> Vec<Vec<IndexEntry>> {
        let after = self.closed.map(|(at, _)| at);
        let mut all: Vec<IndexEntry> = self
            .all
            .iter()
            .filter(|e| after.is_none_or(|at| e.offset > at))
            .copied()
            .collect();
        all.sort_by_key(IndexEntry::order);
        let mut parts: Vec<Vec<IndexEntry>> = Vec::new();
        for track in all.chunk_by(|a, b| a.track_id == b.track_id) {
            for run in track.chunks(FINAL_ENTRIES_MAX) {
                match parts.last_mut() {
                    Some(p)
                        if p.len()
                            .checked_add(run.len())
                            .is_some_and(|n| n <= FINAL_ENTRIES_MAX) =>
                    {
                        p.extend_from_slice(run)
                    }
                    _ => parts.push(run.to_vec()),
                }
            }
        }
        parts
    }

    /// The final index block of `part`, placed at `offset` as block `id`, after the one at `prev`.
    pub(crate) fn final_block(
        part: &[IndexEntry],
        offset: u64,
        prev: Option<u64>,
        id: crate::BlockId,
    ) -> Result<Vec<u8>, IndexError> {
        let head = IndexHead {
            kind: IndexKind::Final,
            self_offset: offset,
            prev,
        };
        let mut buf = vec![0u8; index_len(part.len(), track_count(part))];
        write_index(&mut buf, id, &head, part)?;
        Ok(buf)
    }

    /// The close's rows: each track with a counted chunk or record in the piece, ascending id then owner, with
    /// the item and tick range of its chunks, its hash and its counted records.
    /// Whether track `key` has a row in the close: a chunk or a counted record of it is placed in the piece.
    pub(crate) fn has_row(&self, key: &TrackIdent) -> bool {
        self.rows.contains(key)
    }

    /// The tracks with a row in the close ([Self::has_row]).
    pub(crate) fn rows(&self) -> usize {
        self.rows.len()
    }

    /// The rows the next piece's head carries ([Self::next_starts]) when the tracks `new`, none with a row yet, get
    /// one: each track the head carries or with a row in the close, once.
    pub(crate) fn next_head_rows(&self, new: &BTreeSet<TrackIdent>) -> usize {
        let fresh = new.iter().filter(|k| !self.head_keys.contains(k)).count();
        self.head_keys
            .len()
            .saturating_add(self.rows_past_head)
            .saturating_add(fresh)
    }

    pub(crate) fn track_ranges(&self) -> Vec<TrackRange> {
        let mut rows = self.tracks.clone();
        for (key, t) in self.tallies.iter().filter(|(_, t)| t.touched) {
            let row = rows.entry(*key).or_insert(TrackRange {
                track_id: key.0,
                track_number: t.number,
                chunks: 0,
                first_item: ItemNum::new(0),
                items: 0,
                first_tick: 0,
                last_tick: 0,
                latest_end: 0,
                owner: key.1,
                hash: t.hash,
                records: 0,
                incomplete: false,
            });
            row.hash = t.hash;
            row.records = t.records;
            row.incomplete = t.incomplete;
        }
        // a track with chunks but no unit tallied keeps the hash the head carried, and its mark
        for (key, row) in &mut rows {
            if let Some(t) = self.tallies.get(key).filter(|t| !t.touched) {
                row.hash = t.hash;
                row.incomplete |= t.incomplete;
            }
        }
        // below Integrity::Full no hash is known: zero, and incomplete, so a piece of Full after it says so too
        if self.unhashed {
            for row in rows.values_mut() {
                row.hash = TrackHash::START;
                row.incomplete = true;
            }
        }
        rows.into_values().collect()
    }

    /// Where each track's items and hash continue in a next piece: `prev` (this piece's own starts) moved on by
    /// its chunks and records.
    pub(crate) fn next_starts(&self, prev: &[TrackStart]) -> Vec<TrackStart> {
        next_starts(prev, &self.track_ranges())
    }
}

/// Where each track's items and hash continue in the piece after one whose head said `prev` and whose close lists
/// `ranges`.
pub(crate) fn next_starts<'r>(
    prev: &[TrackStart],
    ranges: impl IntoIterator<Item = &'r TrackRange>,
) -> Vec<TrackStart> {
    // which definitions the next piece carries is its cut's to say
    let mut out: BTreeMap<TrackIdent, TrackStart> = prev
        .iter()
        .map(|t| {
            let t = TrackStart {
                not_carried: false,
                keys_carried: false,
                ..*t
            };
            ((t.track_id, t.owner), t)
        })
        .collect();
    for t in ranges {
        let s = out.entry((t.track_id, t.owner)).or_insert(TrackStart {
            track_id: t.track_id,
            track_number: t.track_number,
            next_item: ItemNum::new(0),
            from_tick: i64::MIN,
            owner: t.owner,
            hash: TrackHash::START,
            incomplete: false,
            not_carried: false,
            keys_carried: false,
        });
        s.track_number = t.track_number;
        s.hash = t.hash;
        s.incomplete |= t.incomplete;
        if t.chunks > 0 {
            s.next_item = t.end_item();
            s.from_tick = s.from_tick.max(t.last_tick);
        }
    }
    out.into_values().collect()
}

/// Add chunk entry `e` to the item and tick range of its track `key`.
fn add_to_range(
    tracks: &mut BTreeMap<TrackIdent, TrackRange>,
    key: TrackIdent,
    track: TrackNum,
    e: &IndexEntry,
) {
    tracks
        .entry(key)
        .and_modify(|t| {
            // a piece of more than u32::MAX chunks keeps the count at u32::MAX
            t.chunks = t.chunks.saturating_add(1);
            t.items = t.first_item.saturating_offset_to(e.end_item());
            t.first_tick = t.first_tick.min(e.first_tick);
            t.last_tick = t.last_tick.max(e.last_tick);
            t.latest_end = t.latest_end.max(e.latest_end);
        })
        .or_insert(TrackRange {
            track_id: e.track_id,
            track_number: track,
            chunks: 1,
            first_item: e.first_item,
            items: e.count,
            first_tick: e.first_tick,
            last_tick: e.last_tick,
            latest_end: e.latest_end,
            owner: key.1,
            hash: TrackHash::START,
            records: 0,
            incomplete: false,
        });
}

/// What a chunk's index entry carries beside what its head says, from its payload and its track's definition.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChunkExtras {
    /// [IndexEntry::unit].
    pub unit: UnitHash,
    /// [IndexEntry::range].
    pub range: Option<(u64, u64)>,
    /// [IndexEntry::tile].
    pub tile: Option<u128>,
}

impl ChunkExtras {
    /// The entry of chunk `c` at `offset`, `len` bytes long, carrying these; None when `len` is over
    /// [MAX_BLOCK_LEN](crate::MAX_BLOCK_LEN).
    pub(crate) fn entry(self, offset: u64, len: usize, c: &Chunk<'_>) -> Option<IndexEntry> {
        IndexEntry::of_chunk(offset, len, &c.head()).map(|e| IndexEntry {
            unit: self.unit,
            range: self.range,
            tile: self.tile,
            relation_break: c.relation_break(),
            ..e
        })
    }

    /// The entries of the good block `block` at `offset`, not a chunk: a summary block's carries the tile number of
    /// these.
    pub(crate) fn summary_entries(self, offset: u64, block: &Block<'_>) -> Vec<IndexEntry> {
        let mut entries = IndexEntry::all_of_block(offset, block);
        if block.kind == BlockKind::Summary {
            for e in &mut entries {
                e.tile = self.tile;
            }
        }
        entries
    }
}

/// The entries of the block `bytes` (exactly one) a writer placed at `offset`: a chunk block's head is checked by its
/// head CRC alone, without a pass over the payload; other blocks by their CRC.
fn of_placed(offset: u64, bytes: &[u8], extras: ChunkExtras) -> Vec<IndexEntry> {
    let chunk = bytes.len() > HEADER_LEN
        && le_u8(bytes, 4) == Some(VERSION)
        && le_u8(bytes, 5).is_some_and(|k| {
            matches!(BlockKind::from_u8(k), BlockKind::Events | BlockKind::Series)
        });
    if chunk {
        let Ok(c) = Chunk::read_head(bytes) else {
            return Vec::new();
        };
        return extras.entry(offset, bytes.len(), &c).into_iter().collect();
    }
    Block::read(bytes).map_or_else(|_| Vec::new(), |b| extras.summary_entries(offset, &b))
}
