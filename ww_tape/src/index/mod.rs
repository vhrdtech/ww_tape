//! Index blocks: where the blocks of a tape piece are, without a scan.
//!
//! An index block ([BlockKind::Index]) is a checkpoint: it lists every block placed since the previous index block
//! of the piece, chunk, summary and record blocks alike, and points back at that previous one, so the
//! chain from the newest checkpoint reaches every block. A writer places one every few MiB ([IndexPolicy]); after a
//! crash a reader finds the newest one from the end of the file and scans only the stretch after it. At the close
//! a **final** index lists every block of the piece again, sorted by track, with a directory of tracks in its head:
//! a reader of one track out of thousands reads the head and that track's entries, each list checked by its own CRC.
//!
//! Offsets are counted from the start of the piece (its [piece head](crate::piece), or where its writer started), so
//! a piece keeps its index when pieces are concatenated. The block header's count is the number of entries; bytes
//! 28..32 hold a CRC of the head, and the body starts with where the entries start, as in a chunk block:
//!
//! ```text
//! off size field
//!  24 u32  session tag, as in every block
//!  28 u32  head CRC: CRC-32C over bytes 0..28, then 32..entries offset
//!  32 u32  entries offset from the block start
//!  36 u32  tracks n in the directory, 0 in a checkpoint
//!  40 u64  self offset: where this block starts in the piece
//!  48 u64  previous index block of the piece (a checkpoint's previous checkpoint, a final block's previous final
//!          block), u64::MAX for none
//!  56 u16  flags: bit 0 final (entries sorted by track id, then offset, state records of one block in block order; a directory
//!          of tracks follows)
//!  58 u16  entry length, 128 (a reader reads an entry's first 120 bytes: it takes longer entries, and refuses
//!          shorter ones than 120)
//!  60 ...  n × track: id u64, first entry u32, entry count u32, CRC-32C of its entries u32, reserved u32
//!  ...     entries, each:
//!            0 u64 offset of the block in the piece    8 u32 its length        12 u8 its block kind
//!           13 u8  summary level (0 for other kinds)   14 u8 summary tree      15 u8 flags: bit 0 repeat, bit 1 range, bit 2 unhashed (no unit hash), bit 3 relation break, bit 4 tile
//!           16 u64 track id (0: none)                 24 u32 track number      28 u32 0 (a state record's below)
//!           32 u64 first item number                  40 u64 items (samples, records)
//!           48 i64 first tick                         56 i64 last tick         64 i64 latest end
//!           72 u64 smallest value of the track's range relation in an events chunk (flag bit 1), else 0
//!           80 u64 its largest value, else 0
//!           72 u128 a grid chunk's tile number (flag bit 4) in place of the range, else 0
//!           88 [32] a chunk's unit hash, else 0
//!          120 [8] reserved: written zero, not read
//! len-4 u32 CRC over bytes 0..len-4, as in every block
//! ```
//!
//! Record blocks of data have no track id, item numbers or ticks (their records may be on many tracks and clocks);
//! their entry says where they are and how many records they hold, under track id 0. A block of state records (a
//! block of definitions, its first record on track 0, or any block marked [FLAG_REPEAT](crate::FLAG_REPEAT)) gets
//! one entry per record instead, under [STATE_TRACK_ID], so a reader finds the item in force at a place, or an item
//! by its hash, without reading the blocks ([state](crate::state)):
//!
//! ```text
//!  13 u8  the record's slot: 0 a state track's record, else what it defines (DefinitionKind)
//!  15 u8  bit 0: the block is marked FLAG_REPEAT
//!  24 u32 the state track, or the number the definition defines (a track's, a clock's, a map's from clock)
//!  28 u32 a map's to clock, else 0
//!  32 u64 0, reserved for the record's item number       40 u64 0, reserved
//!  48 16 bytes the item's hash (ItemHash)                 64 i64 the record's time in ticks, 0 on track 0
//! ```
//!
//! A final index longer than [INDEX_LEN_MAX] is several final blocks, chained like checkpoints, each holding whole
//! tracks in ascending track id order.

use crate::TrackNum;
use crate::bytes::{file_u16, file_u32, le_u16, le_u32, le_u64};
use crate::chunk::u32_len;
use crate::piece::UnitHash;
use crate::state::{ItemHash, Slot};
use crate::{
    Block, BlockError, BlockId, BlockKind, BoundError, CRC, CRC_LEN, ChunkHead, HEADER_LEN,
    MAX_BLOCK_LEN, VERSION, put_header,
};
use crate::{ItemNum, TrackId};

/// Bytes of one index entry a writer writes.
pub const ENTRY_LEN: usize = 128;
/// Bytes of an entry a reader reads, all but the reserved ones: the shortest entry length it takes. Pieces whose
/// entries are this long, written before the entry grew to [ENTRY_LEN], read as they did.
pub const ENTRY_READ_LEN: usize = ENTRY_RESERVED_AT;

/// Where an entry's range ([IndexEntry::range]) or tile number ([IndexEntry::tile]) starts.
const ENTRY_RANGE_AT: usize = 72;
/// Where an entry's unit hash ([UnitHash]) starts.
const ENTRY_UNIT_AT: usize = 88;
/// Where an entry's unit hash ends and its reserved bytes start: zero as written, not read.
const ENTRY_RESERVED_AT: usize = 120;
/// Bytes of one track in a final index's directory.
pub const TRACK_LEN: usize = 24;
/// Bytes of the fixed part of an index head (32..60).
pub const INDEX_FIXED_LEN: usize = 28;
/// Bytes at the start of an index block that say how long its head is ([IndexBlock::head_len]): the header and the
/// entries offset.
pub const HEAD_LEN_PREFIX: usize = HEADER_LEN + 4;
/// Longest index block a writer makes (1 MiB) and a reader scanning back from the end of a file looks for.
pub const INDEX_LEN_MAX: usize = 1 << 20;
/// Most entries in a checkpoint index block.
pub const CHECKPOINT_ENTRIES_MAX: usize =
    (INDEX_LEN_MAX - HEADER_LEN - INDEX_FIXED_LEN - CRC_LEN) / ENTRY_LEN;
/// The track id the index lists state records under, one entry each: no track's identity hash, by convention (a
/// hash of all ones is as likely as any other, and would share the list).
pub const STATE_TRACK_ID: TrackId = TrackId::new(u64::MAX);
/// The track id the index lists record blocks of data under, one entry per block: no track's identity hash, by
/// convention, as [STATE_TRACK_ID].
pub const RECORDS_TRACK_ID: TrackId = TrackId::new(0);
/// Bit 0 of an entry's byte 15: the block is marked [FLAG_REPEAT](crate::FLAG_REPEAT).
const ENTRY_REPEAT: u8 = 1;
/// Entry flag: the entry carries the range of its chunk's range relation.
const ENTRY_RANGE: u8 = 2;
/// Entry flag: no unit hash was computed, set on each entry a writer of
/// [Integrity::CrcOnly](crate::Integrity::CrcOnly) or [Integrity::None](crate::Integrity::None) lists. The unit hash
/// field holds zero ([IndexEntry::unhashed]).
pub const ENTRY_UNHASHED: u8 = 4;
/// Entry flag: the chunk is marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK).
const ENTRY_BREAK: u8 = 8;
/// Entry flag: the entry carries its grid chunk's tile number ([IndexEntry::tile]).
const ENTRY_TILE: u8 = 16;
/// Entry flag bits this build knows; an entry with another set is refused ([IndexError::Invalid]).
const ENTRY_FLAGS_KNOWN: u8 =
    ENTRY_REPEAT | ENTRY_RANGE | ENTRY_UNHASHED | ENTRY_BREAK | ENTRY_TILE;
/// Where an entry's flag byte is.
const ENTRY_FLAGS_AT: usize = 15;
/// Offset that means none.
pub const NONE: u64 = u64::MAX;

const FLAG_FINAL: u16 = 1;
/// Bytes of the header before the head CRC (0..28).
const PRE_CRC_LEN: usize = 28;
/// Where the directory starts: after the header and the fixed part.
const DIR_AT: usize = HEADER_LEN + INDEX_FIXED_LEN;
/// Shortest block an entry may name: a header and a CRC.
const BLOCK_LEN_MIN: usize = HEADER_LEN + CRC_LEN;

/// Whether blocks of `kind` belong to the file structure (index, close, tail, piece head).
///
/// They carry the seq of the last counted block before them and take no part in the seq order, so seq stays the
/// producer's count of its own blocks.
///
/// A reader checks neither order nor gaps on them. Blocks marked [FLAG_REPEAT](crate::FLAG_REPEAT) are stamped the same
/// way.
pub fn is_structural(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::Index
            | BlockKind::Close
            | BlockKind::Tail
            | BlockKind::PieceHead
            | BlockKind::AppendHead
            | BlockKind::CloseSummary
    )
}

/// Where a block is in its piece and what it holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Where the block starts, from the start of the piece.
    pub offset: u64,
    /// The whole block's length.
    pub len: u32,
    pub kind: BlockKind,
    /// A summary block's level, else 0.
    pub level: u8,
    /// A summary block's tree (1 events, 2 series), else 0.
    pub tree: u8,
    /// The track's identity hash ([TrackDef::id](crate::TrackDef::id)); [RECORDS_TRACK_ID] for record blocks of data,
    /// [STATE_TRACK_ID] for a state record.
    pub track_id: TrackId,
    /// Bytes 24..28 as written: a chunk's or summary's track number ([Self::track]), a state record's first slot
    /// number ([Self::state]), else 0.
    pub number: u32,
    /// A state record's second number ([IndexEntry::state]), else 0.
    pub second: u32,
    /// First item number (sample index) the block holds or covers.
    pub first_item: ItemNum,
    /// Items (samples) the block holds or covers; records of a record block.
    pub count: u64,
    pub first_tick: i64,
    pub last_tick: i64,
    pub latest_end: i64,
    /// The block is marked [FLAG_REPEAT](crate::FLAG_REPEAT).
    pub repeat: bool,
    /// The events chunk is marked [FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK): its track's monotone relations
    /// start again, no order across its start.
    pub relation_break: bool,
    /// A chunk's unit hash, which its track's hash takes in; [UnitHash::NONE] for other blocks, for an entry a
    /// reader made by a scan and for an [unhashed](Self::unhashed) entry.
    pub unit: UnitHash,
    /// Its writer computed no unit hash ([ENTRY_UNHASHED]): a piece below
    /// [Integrity::Full](crate::Integrity::Full).
    pub unhashed: bool,
    /// The smallest and the largest value of the chunk's range relation ([RelationIndex::Range](crate::RelationIndex::Range)),
    /// as its writer saw them; None for other blocks, a track without one, and an entry a reader made by a scan.
    pub range: Option<(u64, u64)>,
    /// The tile number of a grid chunk's tile coordinates ([TileLayout::pack](crate::TileLayout::pack)), as its
    /// writer packed them by the grid's definition; None for other blocks, a series chunk (no fixed axes) and an
    /// entry a reader made by a scan.
    pub tile: Option<u128>,
}

/// What an index entry of a state record says, see the [module docs](self).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StateEntry {
    pub slot: Slot,
    pub hash: ItemHash,
    /// The record's time in ticks of its track's clock, 0 on track 0.
    pub ticks: i64,
}

impl IndexEntry {
    /// The entries of the good block `block` at `offset` in its piece: one per record of a block of state records,
    /// else one ([Self::of_block]).
    #[cfg(feature = "alloc")]
    pub fn all_of_block(offset: u64, block: &Block<'_>) -> alloc::vec::Vec<Self> {
        let Some(e) = Self::of_block(offset, block) else {
            return alloc::vec::Vec::new();
        };
        if block.kind != BlockKind::Records || !block.body_known() {
            return alloc::vec![e];
        }
        let mut buf = alloc::vec::Vec::new();
        let Ok(b) = block.decode(&mut buf) else {
            return alloc::vec![e];
        };
        if !b.is_repeat() && b.first_track() != Some(crate::RecordTrack::Definitions) {
            return alloc::vec![e];
        }
        let mut out = alloc::vec::Vec::new();
        for r in b.records() {
            // a record that doesn't read ends the block's records
            let Ok(r) = r else { break };
            let Some(slot) = Slot::of(r.track, r.item) else {
                continue;
            };
            let state = StateEntry {
                slot,
                hash: ItemHash::of(r.item),
                ticks: r.at.map_or(0, |at| at.ticks),
            };
            out.push(e.with_state(&state));
        }
        out
    }

    #[cfg(feature = "alloc")]
    /// This entry of a block, listing the state record `s` in it.
    fn with_state(&self, s: &StateEntry) -> Self {
        let (level, number, second) = s.slot.code();
        let mut halves = [[0u8; 8]; 2];
        for (d, b) in halves.iter_mut().flatten().zip(s.hash.0) {
            *d = b;
        }
        let [a, b] = halves.map(i64::from_le_bytes);
        Self {
            level,
            tree: 0,
            track_id: STATE_TRACK_ID,
            number,
            second,
            first_item: ItemNum::new(0),
            count: 0,
            first_tick: a,
            last_tick: b,
            latest_end: s.ticks,
            ..*self
        }
    }

    #[cfg(feature = "alloc")]
    /// Whether `head` is the chunk this entry names: its kind, track id and number, first item and count. A chunk
    /// read by its entry is compared with it, so an entry pointed at another block reads as damaged, never as that
    /// block.
    pub(crate) fn names_chunk(&self, head: &ChunkHead) -> bool {
        self.kind == head.kind
            && self.track_id == head.track_id
            && self.number == head.track_number.get()
            && self.first_item == head.first_item
            && self.count == u64::from(head.count)
    }

    /// The state record this entry lists, None for an entry of a block.
    pub fn state(&self) -> Option<StateEntry> {
        if self.track_id != STATE_TRACK_ID || self.kind != BlockKind::Records {
            return None;
        }
        let mut hash = [0u8; crate::state::ITEM_HASH_LEN];
        let halves = self
            .first_tick
            .to_le_bytes()
            .into_iter()
            .chain(self.last_tick.to_le_bytes());
        for (d, s) in hash.iter_mut().zip(halves) {
            *d = s;
        }
        let hash = ItemHash(hash);
        Some(StateEntry {
            slot: Slot::from_code(self.level, self.number, self.second, hash)?,
            hash,
            ticks: self.latest_end,
        })
    }

    /// The entry of the good block `bytes` at `offset` in its piece; None for blocks an index doesn't list (index,
    /// close, tail, piece head and parity blocks). A block of state records is listed per record:
    /// [Self::all_of_block].
    pub fn of_block(offset: u64, block: &Block<'_>) -> Option<Self> {
        let bytes = block.bytes();
        // a Block is at most MAX_BLOCK_LEN long: its length fits a u32
        let len = u32::try_from(bytes.len()).ok()?;
        let mut e = Self {
            offset,
            len,
            kind: block.kind,
            level: 0,
            tree: 0,
            track_id: RECORDS_TRACK_ID,
            number: 0,
            second: 0,
            first_item: ItemNum::new(0),
            count: u64::from(block.record_count),
            first_tick: 0,
            last_tick: 0,
            latest_end: 0,
            repeat: false,
            relation_break: false,
            unit: UnitHash::NONE,
            unhashed: false,
            range: None,
            tile: None,
        };
        match block.kind {
            BlockKind::Index
            | BlockKind::Close
            | BlockKind::Tail
            | BlockKind::PieceHead
            | BlockKind::AppendHead
            | BlockKind::CloseSummary
            | BlockKind::Parity => return None,
            BlockKind::Events | BlockKind::Series => {
                if let Some(c) = block.chunk() {
                    return Self::of_chunk(offset, bytes.len(), &c.head()).map(|e| Self {
                        relation_break: c.relation_break(),
                        ..e
                    });
                }
            }
            BlockKind::Summary => {
                if let Some(h) = block.summary_head() {
                    e.track_id = h.track_id;
                    e.number = h.track_number.get();
                    e.tree = h.tree.to_u8();
                    e.level = h.level;
                    e.first_item = h.first_item;
                    e.count = h.items;
                    e.first_tick = h.first_tick;
                    e.last_tick = h.last_tick;
                    e.latest_end = h.latest_end;
                }
            }
            BlockKind::Records => e.repeat = block.is_repeat(),
            _ => {}
        }
        Some(e)
    }

    /// The entry of a chunk block of `len` bytes at `offset` with `head`; None when `len` is over [MAX_BLOCK_LEN].
    pub fn of_chunk(offset: u64, len: usize, head: &ChunkHead) -> Option<Self> {
        if len > MAX_BLOCK_LEN {
            return None;
        }
        Some(Self {
            offset,
            len: u32::try_from(len).ok()?,
            kind: head.kind,
            level: 0,
            tree: 0,
            track_id: head.track_id,
            number: head.track_number.get(),
            second: 0,
            first_item: head.first_item,
            count: u64::from(head.count),
            first_tick: head.first_tick,
            last_tick: head.last_tick,
            latest_end: head.latest_end,
            repeat: false,
            relation_break: false,
            unit: UnitHash::NONE,
            unhashed: false,
            range: None,
            tile: None,
        })
    }

    /// Where the entry goes in a final index: by track id, then offset. The state records of one block share both
    /// and keep their block order (a stable sort).
    pub(crate) fn order(&self) -> (TrackId, u64) {
        (self.track_id, self.offset)
    }

    /// Whether `next` may follow this entry in a final index: a later place, or the next state record of one block.
    pub(crate) fn precedes(&self, next: &Self) -> bool {
        self.order() < next.order()
            || (self.order() == next.order() && self.track_id == STATE_TRACK_ID)
    }

    /// The track of a chunk's or summary's entry; None for an entry of another block or a state record, and for a
    /// damaged entry naming track 0.
    pub fn track(&self) -> Option<TrackNum> {
        let block = matches!(
            self.kind,
            BlockKind::Events | BlockKind::Series | BlockKind::Summary
        );
        if block && self.track_id != STATE_TRACK_ID {
            TrackNum::try_new(self.number)
        } else {
            None
        }
    }

    /// Item numbers past the last one the block holds.
    pub fn end_item(&self) -> ItemNum {
        self.first_item.saturating_add(self.count)
    }

    fn encode(&self) -> [u8; ENTRY_LEN] {
        let mut out = [0u8; ENTRY_LEN];
        let fields = self
            .offset
            .to_le_bytes()
            .into_iter()
            .chain(self.len.to_le_bytes())
            .chain([
                self.kind.to_u8(),
                self.level,
                self.tree,
                (if self.repeat { ENTRY_REPEAT } else { 0 })
                    | (if self.range.is_some() { ENTRY_RANGE } else { 0 })
                    | (if self.tile.is_some() { ENTRY_TILE } else { 0 })
                    | (if self.relation_break { ENTRY_BREAK } else { 0 })
                    | (if self.unhashed { ENTRY_UNHASHED } else { 0 }),
            ])
            .chain(self.track_id.get().to_le_bytes())
            .chain(self.number.to_le_bytes())
            .chain(self.second.to_le_bytes())
            .chain(self.first_item.get().to_le_bytes())
            .chain(self.count.to_le_bytes())
            .chain(self.first_tick.to_le_bytes())
            .chain(self.last_tick.to_le_bytes())
            .chain(self.latest_end.to_le_bytes());
        for (d, s) in out.iter_mut().zip(fields) {
            *d = s;
        }
        if let Some((lo, hi)) = self.range {
            for (d, s) in out
                .iter_mut()
                .skip(ENTRY_RANGE_AT)
                .zip(lo.to_le_bytes().into_iter().chain(hi.to_le_bytes()))
            {
                *d = s;
            }
        }
        if let Some(tile) = self.tile
            && let Some(at) = out.get_mut(ENTRY_RANGE_AT..ENTRY_UNIT_AT)
        {
            at.copy_from_slice(&tile.to_le_bytes());
        }
        if let Some(unit) = out.get_mut(ENTRY_UNIT_AT..ENTRY_RESERVED_AT) {
            unit.copy_from_slice(&self.unit.0);
        }
        out
    }

    /// Read an entry from its first [ENTRY_READ_LEN] bytes and check it, as every reader of an index does: its flags are known
    /// ([IndexError::Invalid]), an unhashed entry holds no unit hash, it names a block
    /// ([IndexError::BadEntries]), a range is a chunk's, its smallest value at most its largest, a tile number a series
    /// chunk's without a range, and a relation break an events chunk's.
    pub(crate) fn read(b: &[u8; ENTRY_READ_LEN]) -> Result<Self, IndexError> {
        let flags = b.get(ENTRY_FLAGS_AT).copied().unwrap_or(0);
        if flags & !ENTRY_FLAGS_KNOWN != 0 {
            return Err(IndexError::Invalid("an entry flag this build doesn't know"));
        }
        let e = Self::decode(b);
        match e.is_valid() {
            true => Ok(e),
            false => Err(IndexError::BadEntries),
        }
    }

    /// Read an entry from its first [ENTRY_READ_LEN] bytes, unchecked: [IndexBlock::read] and [IndexBlock::track_entries] check
    /// each entry they hand out.
    pub fn decode(b: &[u8; ENTRY_READ_LEN]) -> Self {
        #[rustfmt::skip]
        let [
            o0, o1, o2, o3, o4, o5, o6, o7, l0, l1, l2, l3, kind, level, tree, flags,
            t0, t1, t2, t3, t4, t5, t6, t7, n0, n1, n2, n3, r0, r1, r2, r3,
            i0, i1, i2, i3, i4, i5, i6, i7, c0, c1, c2, c3, c4, c5, c6, c7,
            f0, f1, f2, f3, f4, f5, f6, f7, s0, s1, s2, s3, s4, s5, s6, s7,
            e0, e1, e2, e3, e4, e5, e6, e7,
            a0, a1, a2, a3, a4, a5, a6, a7, z0, z1, z2, z3, z4, z5, z6, z7,
            u @ ..
        ] = *b;
        Self {
            offset: u64::from_le_bytes([o0, o1, o2, o3, o4, o5, o6, o7]),
            len: u32::from_le_bytes([l0, l1, l2, l3]),
            kind: BlockKind::from_u8(kind),
            level,
            tree,
            track_id: TrackId::new(u64::from_le_bytes([t0, t1, t2, t3, t4, t5, t6, t7])),
            number: u32::from_le_bytes([n0, n1, n2, n3]),
            second: u32::from_le_bytes([r0, r1, r2, r3]),
            first_item: ItemNum::new(u64::from_le_bytes([i0, i1, i2, i3, i4, i5, i6, i7])),
            count: u64::from_le_bytes([c0, c1, c2, c3, c4, c5, c6, c7]),
            first_tick: i64::from_le_bytes([f0, f1, f2, f3, f4, f5, f6, f7]),
            last_tick: i64::from_le_bytes([s0, s1, s2, s3, s4, s5, s6, s7]),
            latest_end: i64::from_le_bytes([e0, e1, e2, e3, e4, e5, e6, e7]),
            repeat: flags & ENTRY_REPEAT != 0,
            relation_break: flags & ENTRY_BREAK != 0,
            unhashed: flags & ENTRY_UNHASHED != 0,
            unit: UnitHash(u),
            range: (flags & ENTRY_RANGE != 0).then(|| {
                (
                    u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
                    u64::from_le_bytes([z0, z1, z2, z3, z4, z5, z6, z7]),
                )
            }),
            tile: (flags & ENTRY_TILE != 0).then(|| {
                u128::from_le_bytes([
                    a0, a1, a2, a3, a4, a5, a6, a7, z0, z1, z2, z3, z4, z5, z6, z7,
                ])
            }),
        }
    }

    /// Whether the entry names a block ([Self::is_in_range]), its range, if any, is a chunk's and doesn't
    /// decrease, and a tile number is a series chunk's or a series summary block's, never beside a range.
    fn is_valid(&self) -> bool {
        let range = match self.range {
            None => true,
            Some((lo, hi)) => {
                lo <= hi && matches!(self.kind, BlockKind::Events | BlockKind::Series)
            }
        };
        let tile = self.tile.is_none()
            || (self.range.is_none()
                && (self.kind == BlockKind::Series
                    || (self.kind == BlockKind::Summary
                        && self.tree == crate::summary::Tree::Series.to_u8())));
        let mark = !self.relation_break || self.kind == BlockKind::Events;
        let unit = !self.unhashed || self.unit.is_none();
        range && tile && mark && unit && self.is_in_range()
    }

    /// Whether the entry names a block a reader may fetch: its length that of a block, its end within a u64.
    fn is_in_range(&self) -> bool {
        let len = usize::try_from(self.len).ok();
        len.is_some_and(|l| (BLOCK_LEN_MIN..=MAX_BLOCK_LEN).contains(&l))
            && self.offset.checked_add(u64::from(self.len)).is_some()
    }
}

/// A checkpoint or a final index.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IndexKind {
    /// The blocks since the previous checkpoint, in file order.
    Checkpoint,
    /// Blocks sorted by track id, then offset, with a directory of tracks.
    Final,
}

/// The fixed part of an index head.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IndexHead {
    pub kind: IndexKind,
    /// Where this block starts in the piece.
    pub self_offset: u64,
    /// The previous index block of the same kind in the piece.
    pub prev: Option<u64>,
}

/// One track of a final index's directory.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TrackList {
    pub track_id: TrackId,
    /// Index of its first entry in the block.
    pub first: u32,
    pub count: u32,
    /// CRC-32C of its entries' bytes.
    pub crc: u32,
}

impl TrackList {
    fn decode(b: &[u8; TRACK_LEN]) -> Self {
        #[rustfmt::skip]
        let [
            i0, i1, i2, i3, i4, i5, i6, i7,
            f0, f1, f2, f3, n0, n1, n2, n3, c0, c1, c2, c3, _, _, _, _,
        ] = *b;
        Self {
            track_id: TrackId::new(u64::from_le_bytes([i0, i1, i2, i3, i4, i5, i6, i7])),
            first: u32::from_le_bytes([f0, f1, f2, f3]),
            count: u32::from_le_bytes([n0, n1, n2, n3]),
            crc: u32::from_le_bytes([c0, c1, c2, c3]),
        }
    }

    fn encode(&self) -> [u8; TRACK_LEN] {
        let mut out = [0u8; TRACK_LEN];
        let fields = self
            .track_id
            .get()
            .to_le_bytes()
            .into_iter()
            .chain(self.first.to_le_bytes())
            .chain(self.count.to_le_bytes())
            .chain(self.crc.to_le_bytes())
            .chain([0; 4]);
        for (d, s) in out.iter_mut().zip(fields) {
            *d = s;
        }
        out
    }
}

/// Why an index, close, tail or piece head block can't be read or written.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexError {
    Block(BlockError),
    /// The block is of another kind or version.
    Kind,
    /// The head CRC doesn't match.
    BadHeadCrc,
    /// The head or the body is out of range.
    BadHead,
    /// A track's entries don't match their CRC or its list, or an entry names no block (a length outside a block's).
    BadEntries,
    /// A piece head's integrity byte (146) is no [Integrity](crate::piece::Integrity) level.
    UnknownIntegrity {
        level: u8,
    },
    /// The buffer is shorter than the block.
    Full {
        needed: usize,
    },
    /// The entries break a rule of the format (a final index not sorted by track, too many entries).
    Invalid(&'static str),
    /// A count, length or offset of the head is over its bound.
    Bound(BoundError),
    /// A write would list more than `max` state entries in the piece, which lists `used`
    /// ([IndexPolicy::state_entries_max]): the piece is full of state records, a new piece takes them.
    StateEntries {
        max: usize,
        used: usize,
    },
}

impl From<BlockError> for IndexError {
    fn from(e: BlockError) -> Self {
        Self::Block(e)
    }
}

impl core::fmt::Display for IndexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Block(e) => write!(f, "{e}"),
            Self::Kind => write!(f, "a block of another kind or version"),
            Self::BadHeadCrc => write!(f, "the head CRC doesn't match"),
            Self::BadHead => write!(f, "the head or the body is out of range"),
            Self::BadEntries => write!(f, "a track's entries don't match their CRC or list"),
            Self::UnknownIntegrity { level } => {
                write!(f, "integrity byte {level} is no integrity level")
            }
            Self::Full { needed } => write!(f, "the buffer is short: {needed} bytes needed"),
            Self::Invalid(what) => write!(f, "{what}"),
            Self::Bound(e) => write!(f, "{e}"),
            Self::StateEntries { max, used } => write!(
                f,
                "more than {max} state entries in the piece ({used} listed): a new piece takes them"
            ),
        }
    }
}

impl core::error::Error for IndexError {}

impl From<BoundError> for IndexError {
    fn from(e: BoundError) -> Self {
        Self::Bound(e)
    }
}

/// Whether `entries`, by offset, name blocks one after the other that don't overlap: each block once, but a block
/// of state records once per record (state entries of one offset and length).
pub(crate) fn in_file_order<'a>(entries: impl IntoIterator<Item = &'a IndexEntry>) -> bool {
    let mut prev: Option<&IndexEntry> = None;
    for e in entries {
        let fits = prev.is_none_or(|p| match p.offset == e.offset {
            true => p.track_id == STATE_TRACK_ID && e.track_id == STATE_TRACK_ID && p.len == e.len,
            false => p.offset.saturating_add(u64::from(p.len)) <= e.offset,
        });
        if !fits {
            return false;
        }
        prev = Some(e);
    }
    true
}

/// Tracks a final index over `entries` (sorted by track id) has.
pub fn track_count(entries: &[IndexEntry]) -> usize {
    entries.chunk_by(|a, b| a.track_id == b.track_id).count()
}

/// Bytes of an index block with `entries` and `tracks` directory tracks; saturating at `usize::MAX`, a length over
/// [MAX_BLOCK_LEN] no writer takes.
pub const fn index_len(entries: usize, tracks: usize) -> usize {
    (DIR_AT + CRC_LEN)
        .saturating_add(tracks.saturating_mul(TRACK_LEN))
        .saturating_add(entries.saturating_mul(ENTRY_LEN))
}

/// Write the CRC of the block `buf` (exactly its bytes).
///
/// # Panics
///
/// When `buf` is shorter than a block header and its CRC.
pub(crate) fn seal(buf: &mut [u8]) {
    assert!(
        buf.len() >= BLOCK_LEN_MIN,
        "a block of {} bytes is shorter than a header and a CRC",
        buf.len()
    );
    if let Some((body, crc)) = buf.split_last_chunk_mut::<CRC_LEN>() {
        *crc = CRC.checksum(body).to_le_bytes();
    }
}

/// Rewrite the head CRC of the good index block `block` after its header changed (its seq). A block whose head is
/// out of range ([IndexBlock::head_len]) keeps its head CRC: its head reads as bad before and after.
pub(crate) fn restamp_head(block: &mut [u8]) {
    if let Ok(at) = IndexBlock::head_len(block)
        && let Some(crc) = head_crc(block, at)
        && let Some((_, rest)) = block.split_first_chunk_mut::<PRE_CRC_LEN>()
        && let Some((slot, _)) = rest.split_first_chunk_mut::<4>()
    {
        *slot = crc.to_le_bytes();
    }
}

/// CRC-32C over bytes 0..28 and 32..`entries_at`; None when the block is shorter.
fn head_crc(b: &[u8], entries_at: usize) -> Option<u32> {
    let mut d = CRC.digest();
    d.update(b.get(..PRE_CRC_LEN)?);
    d.update(b.get(PRE_CRC_LEN + 4..entries_at)?);
    Some(d.finalize())
}

/// Write an index block listing `entries` into `buf`; returns its length ([index_len]). A final index's entries
/// must be sorted by track id, then offset, the state records of one block in block order.
pub fn write_index(
    buf: &mut [u8],
    id: BlockId,
    head: &IndexHead,
    entries: &[IndexEntry],
) -> Result<usize, IndexError> {
    let tracks = match head.kind {
        IndexKind::Checkpoint => 0,
        IndexKind::Final => {
            if entries
                .windows(2)
                .any(|w| matches!(w, [a, b] if !a.precedes(b)))
            {
                return Err(IndexError::Invalid(
                    "final index not sorted by track and offset",
                ));
            }
            // a reader refuses a track whose blocks overlap (IndexBlock::track_entries), so no writer writes one
            if entries
                .chunk_by(|a, b| a.track_id == b.track_id)
                .any(|t| !in_file_order(t))
            {
                return Err(IndexError::Invalid("a track's blocks overlap"));
            }
            track_count(entries)
        }
    };
    if entries.iter().any(|e| !e.is_in_range()) {
        return Err(IndexError::Invalid("an entry names no block"));
    }
    if entries.iter().any(|e| !e.is_valid()) {
        return Err(IndexError::Invalid(
            "a range that decreases or isn't a chunk's",
        ));
    }
    let len = index_len(entries.len(), tracks);
    let too_long = IndexError::Invalid("index longer than a block");
    if len > MAX_BLOCK_LEN {
        return Err(too_long);
    }
    let count = u32::try_from(entries.len()).ok().ok_or(too_long)?;
    let short = IndexError::Full { needed: len };
    let buf = buf.get_mut(..len).ok_or(short)?;
    // the parts below sum to len, at most MAX_BLOCK_LEN
    let dir_len = tracks.checked_mul(TRACK_LEN).ok_or(too_long)?;
    let entries_at = DIR_AT.checked_add(dir_len).ok_or(too_long)?;
    let (header, rest) = buf.split_first_chunk_mut::<HEADER_LEN>().ok_or(short)?;
    let (fixed, rest) = rest
        .split_first_chunk_mut::<INDEX_FIXED_LEN>()
        .ok_or(short)?;
    let (dir, rest) = rest.split_at_mut_checked(dir_len).ok_or(short)?;
    let (entry_bytes, _) = rest.split_last_chunk_mut::<CRC_LEN>().ok_or(short)?;

    put_header(header, BlockKind::Index, len, count, id).ok_or(too_long)?;
    let flags = match head.kind {
        IndexKind::Checkpoint => 0,
        IndexKind::Final => FLAG_FINAL,
    };
    let entry_len = u16::try_from(ENTRY_LEN).ok().ok_or(too_long)?;
    let fields = u32::try_from(entries_at)
        .ok()
        .ok_or(too_long)?
        .to_le_bytes()
        .into_iter()
        .chain(u32::try_from(tracks).ok().ok_or(too_long)?.to_le_bytes())
        .chain(head.self_offset.to_le_bytes())
        .chain(head.prev.unwrap_or(NONE).to_le_bytes())
        .chain(flags.to_le_bytes())
        .chain(entry_len.to_le_bytes());
    for (d, s) in fixed.iter_mut().zip(fields) {
        *d = s;
    }
    let (slots, _) = entry_bytes.as_chunks_mut::<ENTRY_LEN>();
    for (slot, e) in slots.iter_mut().zip(entries) {
        *slot = e.encode();
    }
    let (dir_slots, _) = dir.as_chunks_mut::<TRACK_LEN>();
    let mut rest: &[u8] = entry_bytes;
    let mut first = 0u32;
    for (slot, track) in dir_slots
        .iter_mut()
        .zip(entries.chunk_by(|a, b| a.track_id == b.track_id))
    {
        let n = track.len().checked_mul(ENTRY_LEN).ok_or(too_long)?;
        let (bytes, r) = rest.split_at_checked(n).ok_or(short)?;
        rest = r;
        let count = u32::try_from(track.len()).ok().ok_or(too_long)?;
        let list = TrackList {
            track_id: track.first().map_or(RECORDS_TRACK_ID, |e| e.track_id),
            first,
            count,
            crc: CRC.checksum(bytes),
        };
        *slot = list.encode();
        first = first.checked_add(count).ok_or(too_long)?;
    }
    let hc = head_crc(buf, entries_at).ok_or(short)?;
    let (_, rest) = buf.split_first_chunk_mut::<PRE_CRC_LEN>().ok_or(short)?;
    let (slot, _) = rest.split_first_chunk_mut::<4>().ok_or(short)?;
    *slot = hc.to_le_bytes();
    seal(buf);
    Ok(len)
}

/// An index block read in place: from a whole block ([Self::read]), or from its head alone ([Self::read_head]) for
/// a reader that fetches only the entries of one track.
#[derive(Copy, Clone, Debug)]
pub struct IndexBlock<'b> {
    /// The block's bytes, or its head's alone
    bytes: &'b [u8],
    len: usize,
    entries_at: usize,
    entry_len: usize,
    entries: usize,
    /// The directory, ascending track id, the lists back to back over the entries
    dir: &'b [[u8; TRACK_LEN]],
    kind: IndexKind,
    self_offset: u64,
    prev: u64,
}

impl<'b> IndexBlock<'b> {
    /// The index of a good block (CRC checked); every entry names a block, and a final index's entries are in the
    /// order [write_index] writes them: by track, then offset, each track's blocks in file order
    /// ([IndexError::BadEntries] else).
    pub fn read(block: &Block<'b>) -> Result<Self, IndexError> {
        if block.kind != BlockKind::Index || !block.body_known() {
            return Err(IndexError::Kind);
        }
        let ix = Self::read_head(block.bytes())?;
        let mut n = 0usize;
        let mut prev: Option<IndexEntry> = None;
        for i in 0..ix.entries {
            let Some(b) = ix.entry_bytes(i) else { break };
            let e = IndexEntry::read(b)?;
            if ix.kind == IndexKind::Final
                && let Some(p) = &prev
                && !(p.precedes(&e) && (p.track_id != e.track_id || in_file_order([p, &e])))
            {
                return Err(IndexError::BadEntries);
            }
            prev = Some(e);
            n = n.saturating_add(1);
        }
        if n != ix.entries {
            return Err(IndexError::BadHead);
        }
        Ok(ix)
    }

    /// Bytes of the head of the index block whose first [HEAD_LEN_PREFIX] bytes are `header`: what [Self::read_head]
    /// needs.
    pub fn head_len(header: &[u8]) -> Result<usize, IndexError> {
        let len = crate::block_len(header)?;
        if header.len() < HEAD_LEN_PREFIX {
            return Err(IndexError::Block(BlockError::Short {
                needed: HEAD_LEN_PREFIX,
            }));
        }
        if header.get(4) != Some(&VERSION)
            || header.get(5).map(|k| BlockKind::from_u8(*k)) != Some(BlockKind::Index)
        {
            return Err(IndexError::Kind);
        }
        let max = len.checked_sub(CRC_LEN).ok_or(IndexError::BadHeadCrc)?;
        let Some(at) = file_u32(header, 32) else {
            return Err(IndexError::BadHeadCrc);
        };
        // read before the head CRC can be checked: out of range, the head can't be trusted
        match at.within(max) {
            Ok(at) if at >= DIR_AT => Ok(at),
            _ => Err(IndexError::BadHeadCrc),
        }
    }

    /// Read the head of an index block from `bytes`, its first [Self::head_len] bytes or more, head CRC checked.
    /// Entries are only read from a whole block or through [Self::track_entries].
    pub fn read_head(bytes: &'b [u8]) -> Result<Self, IndexError> {
        if bytes.len() < HEADER_LEN {
            return Err(IndexError::Block(BlockError::Short { needed: HEADER_LEN }));
        }
        let entries_at = Self::head_len(bytes)?;
        if bytes.len() < entries_at {
            return Err(IndexError::Block(BlockError::Short { needed: entries_at }));
        }
        if head_crc(bytes, entries_at) != le_u32(bytes, 28) {
            return Err(IndexError::BadHeadCrc);
        }
        let len = crate::block_len(bytes)?;
        let bad = IndexError::BadHead;
        let flags = le_u16(bytes, 56).ok_or(bad)?;
        // an index of no entries is shorter than one entry: bounded by the format, not the block
        let entry_len = file_u16(bytes, 58).ok_or(bad)?.at_most(INDEX_LEN_MAX)?;
        let tracks = file_u32(bytes, 36).ok_or(bad)?.within(len)?;
        let entries = file_u32(bytes, 12).ok_or(bad)?.within(len)?;
        let kind = if flags & FLAG_FINAL != 0 {
            IndexKind::Final
        } else {
            IndexKind::Checkpoint
        };
        let dir_end = tracks
            .checked_mul(TRACK_LEN)
            .and_then(|d| d.checked_add(DIR_AT));
        let body = entries
            .checked_mul(entry_len)
            .and_then(|b| b.checked_add(entries_at))
            .and_then(|b| b.checked_add(CRC_LEN));
        if entry_len < ENTRY_READ_LEN
            || dir_end != Some(entries_at)
            || body != Some(len)
            || (kind == IndexKind::Checkpoint && tracks != 0)
        {
            return Err(bad);
        }
        let (dir, _) = bytes.get(DIR_AT..entries_at).ok_or(bad)?.as_chunks();
        let ix = IndexBlock {
            bytes,
            len,
            entries_at,
            entry_len,
            entries,
            dir,
            kind,
            self_offset: le_u64(bytes, 40).ok_or(bad)?,
            prev: le_u64(bytes, 48).ok_or(bad)?,
        };
        // the lists run back to back over the entries, ascending track id
        let mut next = 0usize;
        let mut prev_id = None;
        for t in ix.tracks() {
            let first = crate::FromFile::new(t.first).within(ix.entries)?;
            if first != next || prev_id.is_some_and(|p| p >= t.track_id) {
                return Err(bad);
            }
            let count = crate::FromFile::new(t.count).within(ix.entries)?;
            next = next
                .checked_add(count)
                .filter(|n| *n <= ix.entries)
                .ok_or(bad)?;
            prev_id = Some(t.track_id);
        }
        if kind == IndexKind::Final && next != entries {
            return Err(bad);
        }
        Ok(ix)
    }

    pub fn kind(&self) -> IndexKind {
        self.kind
    }

    /// Where this block starts in its piece.
    pub fn self_offset(&self) -> u64 {
        self.self_offset
    }

    /// The previous index block of the same kind in the piece.
    pub fn prev(&self) -> Option<u64> {
        Some(self.prev).filter(|o| *o != NONE)
    }

    /// The block's whole length.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    pub fn entry_count(&self) -> usize {
        self.entries
    }

    /// Where entry `i` starts in the block; None past the end of the entries.
    pub fn entry_offset(&self, i: usize) -> Option<usize> {
        if i > self.entries {
            return None;
        }
        // at most entries_at + entries × entry_len, the block's length less its CRC
        i.checked_mul(self.entry_len)?.checked_add(self.entries_at)
    }

    /// Bytes of one entry in this block.
    pub fn entry_len(&self) -> usize {
        self.entry_len
    }

    /// Entry `i`, None past the last or when only the head was read.
    pub fn entry(&self, i: usize) -> Option<IndexEntry> {
        self.entry_bytes(i).map(IndexEntry::decode)
    }

    /// The first [ENTRY_READ_LEN] bytes of entry `i`, None past the last or when only the head was read.
    fn entry_bytes(&self, i: usize) -> Option<&'b [u8; ENTRY_READ_LEN]> {
        if i >= self.entries {
            return None;
        }
        let at = self.entry_offset(i)?;
        self.bytes
            .get(at..at.checked_add(self.entry_len)?)?
            .first_chunk::<ENTRY_READ_LEN>()
    }

    /// Every entry; none when only the head was read.
    pub fn entries(&self) -> impl Iterator<Item = IndexEntry> + '_ {
        (0..self.entries).map_while(|i| self.entry(i))
    }

    /// The directory of a final index, ascending track id.
    pub fn tracks(&self) -> impl Iterator<Item = TrackList> + 'b {
        self.dir.iter().map(TrackList::decode)
    }

    /// The directory entry of `track_id`, by binary search.
    pub fn track(&self, track_id: TrackId) -> Option<TrackList> {
        let i = self
            .dir
            .binary_search_by(|t| TrackList::decode(t).track_id.cmp(&track_id))
            .ok()?;
        self.dir.get(i).map(TrackList::decode)
    }

    /// Where `list`'s entries are in the block: offset and length in bytes. A list of this block's directory is
    /// within its entries; the range of any other list saturates past the block, and [Self::track_entries] refuses
    /// it.
    pub fn track_range(&self, list: &TrackList) -> (usize, usize) {
        (
            u32_len(list.first)
                .saturating_mul(self.entry_len)
                .saturating_add(self.entries_at),
            u32_len(list.count).saturating_mul(self.entry_len),
        )
    }

    /// The entries of `list` from their bytes ([Self::track_range]), checked against the list's CRC and the block's
    /// entry count, each checked as [Self::read] checks it, in file order ([IndexError::BadEntries] for entries
    /// that name one block twice or blocks that overlap).
    pub fn track_entries<'e>(
        &self,
        list: &TrackList,
        bytes: &'e [u8],
    ) -> Result<impl Iterator<Item = IndexEntry> + 'e, IndexError> {
        let first = crate::FromFile::new(list.first).within(self.entries)?;
        let count = crate::FromFile::new(list.count).within(self.entries)?;
        if first
            .checked_add(count)
            .is_none_or(|end| end > self.entries)
        {
            return Err(IndexError::BadEntries);
        }
        let (_, n) = self.track_range(list);
        if bytes.len() != n || CRC.checksum(bytes) != list.crc {
            return Err(IndexError::BadEntries);
        }
        // entry_len is at least ENTRY_READ_LEN, checked when the head was read
        let entry = |e: &'e [u8]| e.first_chunk::<ENTRY_READ_LEN>();
        let mut prev = None;
        for e in bytes.chunks_exact(self.entry_len).filter_map(entry) {
            let e = IndexEntry::read(e)?;
            if !in_file_order(prev.iter().chain([&e])) {
                return Err(IndexError::BadEntries);
            }
            prev = Some(e);
        }
        Ok(bytes
            .chunks_exact(self.entry_len)
            .filter_map(entry)
            .map(IndexEntry::decode))
    }
}

/// When a writer places a checkpoint index block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IndexPolicy {
    /// After at least this many bytes of blocks since the previous checkpoint (the rescan a crash costs); 0: never.
    pub bytes: exact_units::Bytes<u64>,
    /// Also when blocks wait unlisted and the previous checkpoint is this long ago, at a block or a writer's tick;
    /// None by default, since a checkpoint placed by time makes the bytes depend on it.
    pub interval: Option<core::time::Duration>,
    /// Most state entries a piece lists, one per state record ([STATE_ENTRIES_MAX]): a write that would list more
    /// is refused ([IndexError::StateEntries]).
    pub state_entries_max: exact_units::Count<exact_units::units::Entry, u64>,
}

/// State entries a piece lists by default ([IndexPolicy::state_entries_max]), each [ENTRY_LEN] bytes of final index.
pub const STATE_ENTRIES_MAX: exact_units::Count<exact_units::units::Entry, u64> =
    exact_units::Count::new(1 << 20);

impl IndexPolicy {
    /// No checkpoints; the final index is still written at the close.
    pub const NONE: Self = Self {
        bytes: exact_units::Bytes::new(0),
        interval: None,
        state_entries_max: STATE_ENTRIES_MAX,
    };
}

impl Default for IndexPolicy {
    fn default() -> Self {
        Self {
            bytes: DEFAULT_CHECKPOINT_BYTES,
            interval: None,
            state_entries_max: STATE_ENTRIES_MAX,
        }
    }
}

/// Default of [IndexPolicy::bytes]: 4 MiB.
pub const DEFAULT_CHECKPOINT_BYTES: exact_units::Bytes<u64> = exact_units::Bytes::new(4 << 20);

#[cfg(feature = "alloc")]
mod build;
#[cfg(feature = "alloc")]
pub(crate) use build::{ChunkExtras, Indexer, TrackIdent, next_starts};

#[cfg(test)]
mod tests;
