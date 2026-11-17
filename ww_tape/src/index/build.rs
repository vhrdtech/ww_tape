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
use crate::piece::{TrackRange, TrackStart};
use crate::{Block, BlockKind, Chunk, HEADER_LEN, MAX_BLOCK_LEN, VERSION};

/// Most entries in one final index block.
const FINAL_ENTRIES_MAX: usize = (MAX_BLOCK_LEN >> 1) / ENTRY_LEN;

/// The blocks of a piece as they are placed: entries not yet in a checkpoint, every entry for the final index,
/// and the item and tick range of each track's chunks.
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
    tracks: BTreeMap<u64, TrackRange>,
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

    /// Note the block `bytes` (exactly one block, written by this writer) placed at `offset` in the piece. Its
    /// CRC is not checked again: a chunk block's head is read by its head CRC.
    pub(crate) fn placed(&mut self, offset: u64, bytes: &[u8]) {
        let entries = of_placed(offset, bytes);
        self.placed_entries(&entries, bytes.len());
    }

    /// Note a `len`-byte block placed whose entries are `entries` (none: a block no index lists; one per state record
    /// of a block of them): also a block a resume finds damaged, listed by the index read back.
    pub(crate) fn placed_entries(&mut self, entries: &[IndexEntry], len: usize) {
        self.placed_unlisted(len);
        for e in entries {
            if matches!(e.kind, BlockKind::Events | BlockKind::Series) {
                add_to_range(&mut self.tracks, e);
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
        if matches!(gone.kind, BlockKind::Events | BlockKind::Series) {
            self.tracks.remove(&gone.track_id);
            for e in self.all.iter().filter(|e| e.track_id == gone.track_id) {
                add_to_range(&mut self.tracks, e);
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

    /// The item and tick range of each track's chunks, ascending track id.
    pub(crate) fn track_ranges(&self) -> Vec<TrackRange> {
        self.tracks.values().copied().collect()
    }

    /// Where each track's items continue in a next piece: `prev` (this piece's own starts) moved on by its chunks.
    pub(crate) fn next_starts(&self, prev: &[TrackStart]) -> Vec<TrackStart> {
        next_starts(prev, self.tracks.values())
    }
}

/// Where each track's items continue in the piece after one whose head said `prev` and whose chunks cover `ranges`.
pub(crate) fn next_starts<'r>(
    prev: &[TrackStart],
    ranges: impl IntoIterator<Item = &'r TrackRange>,
) -> Vec<TrackStart> {
    let mut out: BTreeMap<u64, TrackStart> = prev.iter().map(|t| (t.track_id, *t)).collect();
    for t in ranges {
        let from_tick = out
            .get(&t.track_id)
            .map_or(t.last_tick, |s| s.from_tick.max(t.last_tick));
        out.insert(
            t.track_id,
            TrackStart {
                track_id: t.track_id,
                track_number: t.track_number,
                next_item: t.end_item(),
                from_tick,
            },
        );
    }
    out.into_values().collect()
}

/// Add chunk entry `e` to its track's item and tick range.
fn add_to_range(tracks: &mut BTreeMap<u64, TrackRange>, e: &IndexEntry) {
    tracks
        .entry(e.track_id)
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
        });
}

/// The entries of the block `bytes` (exactly one) a writer placed at `offset`: a chunk block's head is checked by its
/// head CRC alone, without a pass over the payload; other blocks by their CRC.
fn of_placed(offset: u64, bytes: &[u8]) -> Vec<IndexEntry> {
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
            .into_iter()
            .collect();
    }
    Block::read(bytes).map_or_else(|_| Vec::new(), |b| IndexEntry::all_of_block(offset, &b))
}
