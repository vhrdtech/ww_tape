//! [Indexer]: what a writer keeps to place checkpoints and write the final index and the finish.

use alloc::collections::BTreeMap;
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
pub(crate) type TrackKey = (u64, OwnerId);

/// A track's hash and counted records in a piece.
#[derive(Copy, Clone, Debug)]
struct Tally {
    number: u32,
    hash: TrackHash,
    records: u64,
    /// The piece holds a counted chunk or record of the track: the finish lists it
    touched: bool,
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
    last_checkpoint: Option<u64>,
    tracks: BTreeMap<TrackKey, TrackRange>,
    /// The track each number stands for, from the definitions placed in the piece
    numbers: BTreeMap<u32, TrackKey>,
    /// The tracks the piece head carries over, by number: where a number has no definition placed (a damaged one)
    carried: BTreeMap<u32, TrackKey>,
    tallies: BTreeMap<TrackKey, Tally>,
    /// The tallies before the last counted block of data, to take it back ([Self::undo_block])
    undo: Vec<(TrackKey, Option<Tally>)>,
}

impl Indexer {
    pub(crate) fn new(policy: IndexPolicy) -> Self {
        Indexer {
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
            self.tallies.insert(
                key,
                Tally {
                    number: t.track_number,
                    hash: t.hash,
                    records: 0,
                    touched: false,
                },
            );
        }
    }

    /// Track number `number` stands for track `id` of `owner` from here on: a track definition was placed.
    pub(crate) fn define(&mut self, number: u32, id: u64, owner: OwnerId) {
        self.numbers.insert(number, (id, owner));
    }

    /// The track of number `number`, its id `id` when the block says it: by the definition placed in the piece, else
    /// by the head; a chunk whose number names no track of its id is of the piece's owner.
    fn key_of(&self, number: u32, id: Option<u64>) -> Option<TrackKey> {
        let fits = |k: &&TrackKey| id.is_none_or(|id| k.0 == id);
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
        for (key, before) in self.undo.drain(..).rev() {
            match before {
                Some(t) => self.tallies.insert(key, t),
                None => self.tallies.remove(&key),
            };
        }
    }

    fn tally(
        &mut self,
        key: TrackKey,
        number: u32,
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
        });
        t.number = number;
        t.hash = unit(&t.hash);
        t.records = t.records.saturating_add(records);
        t.touched = true;
        self.tallies.insert(key, t);
    }

    /// The counted chunk with `head` and unit hash `unit` goes into its track's hash.
    pub(crate) fn tally_chunk(&mut self, head: &crate::ChunkHead, unit: &UnitHash) {
        if let Some(key) = self.key_of(head.track_number, Some(head.track_id)) {
            self.tally(key, head.track_number, |h| h.then(unit), 0);
        }
    }

    /// The counted record on track `number` at `ticks` holding `item` goes into its track's hash; a number no
    /// definition in the piece or the head names is left out.
    pub(crate) fn tally_record(&mut self, number: u32, ticks: i64, item: &[u8]) {
        if let Some(key) = self.key_of(number, None) {
            self.tally(key, number, |h| h.record(ticks, item), 1);
        }
    }

    /// Note the block `bytes` (exactly one block, written by this writer) placed at `offset` in the piece, a chunk
    /// with its unit hash `unit` and its range relation's smallest and largest value `range`. Its CRC is not checked
    /// again: a chunk block's head is read by its head CRC.
    pub(crate) fn placed(
        &mut self,
        offset: u64,
        bytes: &[u8],
        unit: UnitHash,
        range: Option<(u64, u64)>,
    ) {
        let entries = of_placed(offset, bytes, unit, range);
        self.placed_entries(&entries, bytes.len());
    }

    /// Note a `len`-byte block placed whose entries are `entries` (none: a block no index lists; one per state record
    /// of a block of them): also a block a resume finds damaged, listed by the index read back.
    pub(crate) fn placed_entries(&mut self, entries: &[IndexEntry], len: usize) {
        self.placed_unlisted(len);
        for e in entries {
            if matches!(e.kind, BlockKind::Events | BlockKind::Series)
                && let Some(key) = self.key_of(e.track_number, Some(e.track_id))
            {
                add_to_range(&mut self.tracks, key, e);
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
        let max = self.policy.state_entries_max;
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
            && let Some(key) = self.key_of(gone.track_number, Some(gone.track_id))
        {
            self.tracks.remove(&key);
            let same: Vec<IndexEntry> = self
                .all
                .iter()
                .filter(|e| {
                    matches!(e.kind, BlockKind::Events | BlockKind::Series)
                        && self.key_of(e.track_number, Some(e.track_id)) == Some(key)
                })
                .copied()
                .collect();
            for e in &same {
                add_to_range(&mut self.tracks, key, e);
            }
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
            && ((self.policy.bytes > 0 && self.since >= self.policy.bytes)
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
            prev: self.last_checkpoint,
        };
        let n = self.pending.len().min(CHECKPOINT_ENTRIES_MAX);
        let listed = self.pending.get(..n).unwrap_or_default();
        let mut buf = vec![0u8; index_len(listed.len(), 0)];
        write_index(&mut buf, id, &head, listed)?;
        self.pending.drain(..n);
        self.since = 0;
        self.last_checkpoint = Some(offset);
        Ok(buf)
    }

    /// The newest checkpoint's offset.
    pub(crate) fn last_checkpoint(&self) -> Option<u64> {
        self.last_checkpoint
    }

    /// Every entry, sorted for the final index, cut into the parts one final block each holds: whole tracks while
    /// they fit, a track longer than a block over several.
    pub(crate) fn final_parts(&self) -> Vec<Vec<IndexEntry>> {
        let mut all = self.all.clone();
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

    /// The finish's rows: each track with a counted chunk or record in the piece, ascending id then owner, with
    /// the item and tick range of its chunks, its hash and its counted records.
    pub(crate) fn track_ranges(&self) -> Vec<TrackRange> {
        let mut rows = self.tracks.clone();
        for (key, t) in self.tallies.iter().filter(|(_, t)| t.touched) {
            let row = rows.entry(*key).or_insert(TrackRange {
                track_id: key.0,
                track_number: t.number,
                chunks: 0,
                first_item: 0,
                items: 0,
                first_tick: 0,
                last_tick: 0,
                latest_end: 0,
                owner: key.1,
                hash: t.hash,
                records: 0,
            });
            row.hash = t.hash;
            row.records = t.records;
        }
        // a chunk the hashes left out (a damaged one a resume kept) keeps the hash the head carried
        for (key, row) in &mut rows {
            if let Some(t) = self.tallies.get(key).filter(|t| !t.touched) {
                row.hash = t.hash;
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

/// Where each track's items and hash continue in the piece after one whose head said `prev` and whose finish lists
/// `ranges`.
pub(crate) fn next_starts<'r>(
    prev: &[TrackStart],
    ranges: impl IntoIterator<Item = &'r TrackRange>,
) -> Vec<TrackStart> {
    let mut out: BTreeMap<TrackKey, TrackStart> =
        prev.iter().map(|t| ((t.track_id, t.owner), *t)).collect();
    for t in ranges {
        let s = out.entry((t.track_id, t.owner)).or_insert(TrackStart {
            track_id: t.track_id,
            track_number: t.track_number,
            next_item: 0,
            from_tick: i64::MIN,
            owner: t.owner,
            hash: TrackHash::START,
        });
        s.track_number = t.track_number;
        s.hash = t.hash;
        if t.chunks > 0 {
            s.next_item = t.end_item();
            s.from_tick = s.from_tick.max(t.last_tick);
        }
    }
    out.into_values().collect()
}

/// Add chunk entry `e` to the item and tick range of its track `key`.
fn add_to_range(tracks: &mut BTreeMap<TrackKey, TrackRange>, key: TrackKey, e: &IndexEntry) {
    tracks
        .entry(key)
        .and_modify(|t| {
            // a piece of more than u32::MAX chunks keeps the count at u32::MAX
            t.chunks = t.chunks.saturating_add(1);
            t.items = e.end_item().saturating_sub(t.first_item);
            t.first_tick = t.first_tick.min(e.first_tick);
            t.last_tick = t.last_tick.max(e.last_tick);
            t.latest_end = t.latest_end.max(e.latest_end);
        })
        .or_insert(TrackRange {
            track_id: e.track_id,
            track_number: e.track_number,
            chunks: 1,
            first_item: e.first_item,
            items: e.count,
            first_tick: e.first_tick,
            last_tick: e.last_tick,
            latest_end: e.latest_end,
            owner: key.1,
            hash: TrackHash::START,
            records: 0,
        });
}

/// The entries of the block `bytes` (exactly one) a writer placed at `offset`: a chunk block's head is checked by its
/// head CRC alone, without a pass over the payload; other blocks by their CRC.
fn of_placed(
    offset: u64,
    bytes: &[u8],
    unit: UnitHash,
    range: Option<(u64, u64)>,
) -> Vec<IndexEntry> {
    let chunk = bytes.len() > HEADER_LEN
        && le_u8(bytes, 4) == Some(VERSION)
        && le_u8(bytes, 5).is_some_and(|k| {
            matches!(BlockKind::from_u8(k), BlockKind::Events | BlockKind::Series)
        });
    if chunk {
        let Ok(c) = Chunk::read_head(bytes) else {
            return Vec::new();
        };
        return IndexEntry::of_chunk(offset, bytes.len(), &c.head())
            .map(|e| IndexEntry { unit, range, ..e })
            .into_iter()
            .collect();
    }
    Block::read(bytes).map_or_else(|_| Vec::new(), |b| IndexEntry::all_of_block(offset, &b))
}
