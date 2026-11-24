//! Tape pieces: one recording cut into complete files, and how a piece ends.
//!
//! A **piece** is a complete tape file: a [piece head](PieceHead) block, blocks of definitions, chunk, summary and
//! record blocks with checkpoint [index](crate::index) blocks between them, then at the close a final index, a
//! [close record](Close) and a fixed-size [tail](Tail) block. Any piece reads alone: a writer puts the definitions
//! each block of data needs close behind it in the same piece ([state](crate::state)), and right after the head those
//! of every track the head carries, quiet ones too ([TapeWriter::cut_on](crate::TapeWriter::cut_on)); pieces written
//! back to back are one valid tape. A file a writer started without a piece head is one piece too, with no ids.
//!
//! A writer cuts a recording into pieces at chunk boundaries ([PieceChain]). The head names the previous piece by its
//! content hash and its id; the previous piece's tail names this piece's id, derived at the cut from the previous
//! piece's id and the content hash in its close record ([PieceId::after]), so the chain is checked both ways
//! ([check_chain]). Each piece carries the owner id, its sequence number in the chain and, per track, where its items
//! continue (head) and the items and ticks it holds (close record). A chain whose first piece is gone reads as head
//! cut, not as damage.
//!
//! The close record is the last thing a writer places before the tail. Its content hash is BLAKE3 over the piece's
//! bytes before it; a piece without one is live or was cut off, and reads up to its last good block. The tail sits
//! at the very end of the file, [TAIL_LEN] bytes, so a reader finds it with one read and from it the close record,
//! the indexes and the start of the piece; the tail of the piece before is just before that start.
//!
//! **Closing more than once.** A close record ends a write session, not the piece: a writer may write on after it
//! ([TapeWriter::resume_on](crate::TapeWriter::resume_on)). It places an [append head](AppendHead) right after the
//! tail, naming the close record, the tail and the close hash it goes on from, then any blocks, then a final index
//! listing the blocks since that close and chained back to its final index, a new close record and a new tail whose
//! piece length spans the whole piece. That final index is at least one block, an empty one when no block came since:
//! each close after the first follows a final index block of its own, so a reader walking the index chain back from
//! a checkpoint takes the close after the newest final block it meets. Nothing before the append head changes:
//!
//! ```text
//! piece head, blocks, checkpoints, final index, close 1, tail 1      closed once, a multiple of 4 KiB
//! append head naming close 1 and tail 1
//! blocks, checkpoints, final index, close 2 (previous: close 1), tail 2      closed twice
//! ```
//!
//! Only the last piece of a chain is written on, so a close that names a next piece is never followed by an append
//! head, and links never move.
//!
//! - The last good close record is the one in force; each earlier one stays in the bytes, its hash still that of the
//!   piece up to it. A later close record's hash is BLAKE3 of the previous one's hash, then of the bytes from the
//!   previous close record to this one, each chunk payload by its hash: the writer chains over the hash the previous
//!   close states, never rereading what is before it. Each close names the one before ([Close::prev]).
//! - Bytes after the last tail are an unclosed tail: a writer writing now, or one a crash stopped. A reader takes the
//!   piece as closed by its last close, and reads the good blocks after it as it reads an unclosed piece: never as
//!   damage of what the close covers. The next writer keeps those blocks and cuts back only a torn end.
//! - A summary of the caller's own: a close may place one block ([write_close_summary], kind 14) right before its final
//!   index, its body the caller's and at most [CLOSE_SUMMARY_MAX] bytes. The close record names it by offset and
//!   length ([Close::summary]) and its hash covers it, so a reader of the tail finds it with one more read
//!   ([TapeReader::close_summary](crate::TapeReader::close_summary)) and a verify checks it.
//! - Pieces removed on purpose: a close may name pieces of its chain that were removed on purpose, at most
//!   [REMOVED_PIECES_MAX] ([RemovedPiece]: sequence number, id and the content hash of its close in force). The writer
//!   names them again in every later close of the piece
//!   ([TapeWriter::record_removed](crate::TapeWriter::record_removed)). [check_chain] links over removed pieces when
//!   the piece before names the first by id and the piece after names the last by hash and id
//!   ([ChainLink::Removed]); a chain whose first pieces were removed so reads as removed, not head cut.
//!
//! A closed piece is a multiple of [PIECE_ALIGN] (4 KiB) long: the writer pads the close record's body with zeros
//! ([close_padding]), so the tail, still the piece's last bytes, ends on the boundary. Pieces then clone back to back
//! (reflinks on btrfs or XFS) with nothing between them. Only the end is aligned; the blocks inside stay where they
//! are, and a piece without a close ends where it ends.
//!
//! Layouts after the 32-byte block header (the header's count: tracks), little endian:
//!
//! ```text
//! piece head (kind 10)
//!  32 [16] piece id                 48 [32] owner id                80 u64 sequence number in the chain
//!  88 u32  flags: bit 0 has a previous piece                        92 u32 previous session tag (0: none or unknown)
//!  96 [32] previous piece's content hash                           128 [16] previous piece's id
//! 144 u16  track row length, 96 (a reader takes longer rows and reads their first 96 bytes)
//! 146 u8   integrity (Integrity): 0 Full, 1 CrcOnly, 2 None (a reader refuses another value)    147 [5] reserved
//! 152 ...  tracks × 96, one per track carried over from the pieces before:
//!            0 u64 id    8 u32 number
//!           12 u32 flags, as a close row's: bit 0 incomplete (from the pieces before); bit 1, a head row's only:
//!                  not carried, the piece holds no definition of the track, its cut had no room for it; bit 2, a
//!                  head row's only: keys carried, the piece carries every live key of this keyed state track
//!           16 u64 next item (the first item number this piece holds)
//!           24 i64 from tick (no item of the track here starts before it)
//!           32 [32] owner (TrackDef::owner; zeros: the piece's owner)
//!           64 [32] track hash before this piece ([TrackHash])
//!
//! close (kind 8)
//!  32 u8   reason (1 closed, 2 aborted; a piece that goes on in the next one, next id below, is closed)
//!  33 u8   reserved
//!  34 u16  track row length, 136 (a reader takes longer rows and reads their first 136 bytes)
//!  36 u16  removed-piece row length, 56 (a reader takes longer rows and reads their first 56 bytes; 0 with no rows)
//!  38 [2]  reserved
//!  40 [32] content hash: BLAKE3 of the piece's bytes before this block, each chunk payload by its hash (below); of
//!          a later close, BLAKE3 of the previous close's hash, then of the bytes from it to this block, the same way
//!  72 u64  newest checkpoint index offset    80 u64 last final index offset
//!  88 u64  previous close record's offset (u64::MAX: the piece's first close)
//!          (offsets u64::MAX for none)
//!  96 [16] piece id    112 [16] next piece id    128 [32] owner id    160 u64 sequence number
//! 168 u32  flags: bit 0 the piece has a head    172 u32 padding: zero bytes after the rows, below [PIECE_ALIGN]
//! 176 u64  close summary offset, u64::MAX for none    184 u32 its length
//! 188 u32  removed pieces, at most REMOVED_PIECES_MAX: their rows follow the tracks
//! 192 ...  tracks × 136, one per track with a counted chunk or record in the piece, ascending id then owner:
//!            0 u64 id    8 u32 number    12 u32 chunks (0: records only, the range fields below 0)
//!           16 u64 first item    24 u64 items    32 i64 first tick    40 i64 last tick
//!           48 i64 latest end (of the chunk blocks in the piece)
//!           56 [32] owner (TrackDef::owner)
//!           88 [32] track hash after this piece ([TrackHash])
//!          120 u64 records: the track's counted records in the piece
//!          128 u32 flags: bit 0 incomplete, the track hash leaves out units a resume kept unread    132 u32 reserved, 0
//!     ...  removed pieces × 56, ascending distinct sequence numbers (RemovedPiece):
//!            0 u64 sequence number    8 [16] piece id    24 [32] content hash of its close in force
//!     ...  padding zeros, covered by the CRC
//!
//! tail (kind 9), TAIL_LEN bytes, the last of the piece
//!  32 u64  piece length, from the piece's start to the end of this block
//!  40 u64  close offset    48 u32 close length    52 u8 reason    53 u8 flags (as the close's)    54 u16 reserved
//!  56 [32] content hash    88 [16] piece id    104 [16] next piece id    120 [32] owner id    152 u64 sequence
//! 160 u32  CRC
//!
//! close summary (kind 14), right before the final index of the close naming it
//!  32 [n]  the caller's body, at most CLOSE_SUMMARY_MAX bytes    32 + n u32 CRC
//!
//! append head (kind 13), APPEND_HEAD_LEN bytes, right after the tail of the close it goes on from
//!  32 u64  that close record's offset    40 u64 that tail's offset    48 [32] that close record's content hash
//!  80 u32  CRC
//! ```
//!
//! Offsets are from the start of the piece. Ids are opaque to `ww_tape`, their meaning the application's: a piece id
//! is 16 bytes, an owner id 32 (e.g. a hash the producer derives); all zeros is none. A track is told apart by its id
//! and its owner ([TrackDef::owner](crate::TrackDef::owner)): two owners' tracks of one identity share a file, each
//! with its own number, item numbers and hash.
//!
//! **Content hash.** BLAKE3 over the piece's bytes before its close record, in order, except that the payload of
//! every chunk block that reads whole (from its payload offset to its CRC: the columns and their 16 zero bytes) is
//! replaced by the 32-byte BLAKE3 hash of that payload. Every byte is covered; the payload is hashed once, for this
//! hash and the track hash both. The blocks are found from the start: where a block reads whole it is taken whole,
//! else the bytes up to the next place holding the block magic go in as they are. A place is tried as a block only
//! while the blocks tried that didn't read whole claim at most [WALK_MISS_PER_BYTE_MAX] bytes per byte gone plus
//! [WALK_MISS_BASE_MAX], so crafted bytes cost a walk a bounded multiple of their length. Damaged bytes so hash the
//! same whichever way a resume or a reader walks them.
//!
//! **Track hash.** Each track has a running hash ([TrackHash]) over its content as written, in the order written:
//! 32 zero bytes before its first, then `h = BLAKE3(h || BLAKE3(unit))` for each unit. The inner hash is the unit
//! hash ([UnitHash]); a chunk's index entry stores it, so the hash of a run of whole chunks folds from the index alone
//! ([TapeReader::range_hash](crate::TapeReader::range_hash)). A unit is:
//!
//! ```text
//! chunk:  01, kind u8, count u32, the head's bytes 48..80 (first item, first tick, last tick, latest end) and
//!         81..end of the column entries (columns, time mode, end mode, a series grid and scale, the column
//!         entries), then the payload's BLAKE3 hash
//! record: 02, ticks i64 (0 without a time), item length u64, the item's bytes
//! ```
//!
//! Units are the counted blocks only: chunk blocks and the records of record blocks, never a block marked
//! [FLAG_REPEAT](crate::FLAG_REPEAT) (puts and copies), a summary, a definition or a block of the file structure.
//! Nothing of where a block lies is hashed (session, seq, track number, payload offset, padding, segment CRCs), so a
//! track written alone or among others, in one piece or many, in one file or many, has the same hash. A state
//! track's records are all puts: its hash stays 32 zero bytes.

use crate::{ItemNum, TrackId};
#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use crate::TrackNum;
use crate::bytes::{
    array, le_i64, le_u8, le_u16, le_u32, le_u64, put, put_i64, put_u16, put_u32, put_u64,
};
use crate::chunk::{len_u64, to_boundary, u32_len};
use crate::index::{IndexError, NONE, seal};
use crate::{Block, BlockId, BlockKind, CRC_LEN, HEADER_LEN, SessionTag, put_header};

/// Bytes of a tail block.
pub const TAIL_LEN: usize = 164;
/// A closed piece's length is a multiple of this: 4 KiB, [crate::ALIGNMENT].
pub const PIECE_ALIGN: u32 = crate::ALIGNMENT;
const _: () = assert!(PIECE_ALIGN.is_power_of_two(), "padding takes the low bits");
/// Bytes of the fixed part of a piece head body.
const HEAD_FIXED_LEN: usize = 120;
/// Bytes of a piece head's track row this crate writes; a reader takes longer ones.
const HEAD_TRACK_LEN: usize = 96;
/// Where a piece head names its row length.
const HEAD_ROW_LEN_AT: usize = 144;
/// Where a piece head's integrity level is, an [Integrity] byte.
const HEAD_INTEGRITY_AT: usize = 146;
/// Where a piece head's track row holds its flags, those of a close's row ([ROW_INCOMPLETE]).
const HEAD_ROW_FLAGS_AT: usize = 12;
/// Where a piece head's tracks start.
const HEAD_TRACKS_AT: usize = HEADER_LEN + HEAD_FIXED_LEN;
/// Bytes of the fixed part of a close body.
const CLOSE_FIXED_LEN: usize = 160;
/// Bytes of a close's track row this crate writes; a reader takes longer ones.
const CLOSE_TRACK_LEN: usize = 136;
/// Bytes of a close's removed-piece row this crate writes; a reader takes longer ones.
const REMOVED_ROW_LEN: usize = 56;
/// Where a close names its removed-piece row length.
const REMOVED_ROW_LEN_AT: usize = 36;
/// Where a close names how many removed-piece rows it holds.
const REMOVED_COUNT_AT: usize = 188;
/// Most pieces one close names as removed ([RemovedPiece]): 65536, 3.5 MiB of rows.
///
/// A writer refuses more ([crate::RemovedRefusal::TooMany]), a reader refuses a close stating more.
pub const REMOVED_PIECES_MAX: usize = 1 << 16;
/// Most tracks with data in one piece: the close's rows of that many and [REMOVED_PIECES_MAX] removed-piece rows,
/// padded to [PIECE_ALIGN], fit a block ([crate::MAX_BLOCK_LEN]).
///
/// A writer refuses a block that would bring in one more ([crate::WriteError]).
pub const TRACKS_PER_PIECE_MAX: usize = (crate::MAX_BLOCK_LEN
    - HEADER_LEN
    - CLOSE_FIXED_LEN
    - CRC_LEN
    - (u32_len(PIECE_ALIGN) - 1)
    - REMOVED_PIECES_MAX * REMOVED_ROW_LEN)
    / CLOSE_TRACK_LEN;
const _: () = assert!(
    close_len(TRACKS_PER_PIECE_MAX, REMOVED_PIECES_MAX) + u32_len(PIECE_ALIGN) - 1
        <= crate::MAX_BLOCK_LEN,
    "the close of the most tracks and removed pieces fits a block"
);
/// Most track rows one piece head carries: each track the chain's pieces before it carried or gave data, so a head of
/// that many fits a block ([crate::MAX_BLOCK_LEN]), padded as a close is.
///
/// A writer refuses a block that would bring the next head past it ([crate::WriteError]).
pub const HEAD_TRACKS_MAX: usize =
    (crate::MAX_BLOCK_LEN - HEADER_LEN - HEAD_FIXED_LEN - CRC_LEN - (u32_len(PIECE_ALIGN) - 1))
        / HEAD_TRACK_LEN;
const _: () = assert!(
    piece_head_len(HEAD_TRACKS_MAX) + u32_len(PIECE_ALIGN) - 1 <= crate::MAX_BLOCK_LEN,
    "the head of the most tracks fits a block"
);
/// Where a close's track row holds its flags.
const ROW_FLAGS_AT: usize = 128;
/// A track row's flag: the track hash leaves out units a resume kept unread.
const ROW_INCOMPLETE: u32 = 1;
/// A piece head's track row flag: the piece doesn't carry the track's definition, its cut had no room for it
/// ([TrackStart::not_carried]). A close's row never holds it.
const ROW_NOT_CARRIED: u32 = 2;
/// A piece head's track row flag on a keyed state track: the piece carries every key live at its start
/// ([TrackStart::keys_carried]). A close's row never holds it.
const ROW_KEYS_CARRIED: u32 = 4;
/// Where a close names its row length.
const CLOSE_ROW_LEN_AT: usize = 34;
/// Where a close record's tracks start.
const CLOSE_TRACKS_AT: usize = HEADER_LEN + CLOSE_FIXED_LEN;

const FLAG_HAS_PREV: u32 = 1;
const FLAG_PIECE: u8 = 1;

/// Names a piece of a chain.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PieceId(pub [u8; 16]);

/// Names the owner of a piece, opaque to `ww_tape`: an application reads it its own way.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct OwnerId(pub [u8; 32]);

impl OwnerId {
    /// No owner of its own: all zeros. On a track, the piece's owner.
    pub const NONE: Self = Self([0; 32]);

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }
}

/// BLAKE3 of a piece's bytes before its close record, each chunk payload by its hash (see the [module docs](self)).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ContentHash(pub [u8; 32]);

/// A track's running content hash, see the [module docs](self#track-hash).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TrackHash(pub [u8; 32]);

/// BLAKE3 of one unit of a track's content, a chunk or a record (see the [module docs](self#track-hash)); all zeros
/// in an index entry: not known.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct UnitHash(pub [u8; 32]);

impl UnitHash {
    /// No unit hash: what an index entry of a block other than a chunk, or one a reader made by a scan, holds.
    pub const NONE: Self = Self([0; 32]);

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }
}

/// A unit of a chunk in [TrackHash].
const UNIT_CHUNK: u8 = 1;
/// A unit of a record in [TrackHash].
const UNIT_RECORD: u8 = 2;

impl TrackHash {
    /// The hash of a track with nothing written yet: 32 zero bytes.
    pub const START: Self = Self([0; 32]);

    /// The hash after the unit whose hash is `unit` ([Self::chunk_unit], [Self::record_unit]).
    pub fn then(&self, unit: &UnitHash) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(&self.0);
        h.update(&unit.0);
        Self(*h.finalize().as_bytes())
    }

    /// The hash after the chunk `chunk`, whose payload hashes to `payload` ([payload_hash]).
    pub fn chunk(&self, chunk: &crate::Chunk<'_>, payload: &[u8; 32]) -> Self {
        self.then(&Self::chunk_unit(chunk, payload))
    }

    /// The hash after a record at `ticks` (0 without a time) holding `item`.
    pub fn record(&self, ticks: i64, item: &[u8]) -> Self {
        self.then(&Self::record_unit(ticks, item))
    }

    /// The unit hash of the chunk `chunk`, whose payload hashes to `payload`: what its index entry stores.
    pub fn chunk_unit(chunk: &crate::Chunk<'_>, payload: &[u8; 32]) -> UnitHash {
        let head = chunk.head();
        let (times, columns) = chunk.descriptor();
        let mut h = blake3::Hasher::new();
        h.update(&[UNIT_CHUNK, head.kind.to_u8()]);
        h.update(&head.count.to_le_bytes());
        h.update(times);
        h.update(columns);
        h.update(payload);
        UnitHash(*h.finalize().as_bytes())
    }

    /// The unit hash of a record at `ticks` (0 without a time) holding `item`.
    pub fn record_unit(ticks: i64, item: &[u8]) -> UnitHash {
        let mut h = blake3::Hasher::new();
        h.update(&[UNIT_RECORD]);
        h.update(&ticks.to_le_bytes());
        h.update(&crate::chunk::len_u64(item.len()).to_le_bytes());
        h.update(item);
        UnitHash(*h.finalize().as_bytes())
    }
}

/// BLAKE3 of a chunk's payload ([crate::Chunk::payload]): what stands for it in the content hash and the track hash.
pub fn payload_hash(payload: &[u8]) -> [u8; 32] {
    *blake3::hash(payload).as_bytes()
}

/// [payload_hash] of the payload a chunk of `columns` gets: the columns back to back, then the 16 zero bytes of
/// padding. What a producer computes on its own thread for [TapeWriter::write_chunk_hashed](crate::TapeWriter::write_chunk_hashed).
pub fn columns_payload_hash<'c>(columns: impl IntoIterator<Item = &'c [u8]>) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for c in columns {
        h.update(c);
    }
    h.update(&[0u8; crate::chunk::PAYLOAD_PADDING]);
    *h.finalize().as_bytes()
}

#[cfg(feature = "alloc")]
/// Feed the good block `block` to a piece's content hash: a chunk's payload by its hash, which is returned with the
/// chunk; any other block, or one that doesn't read as a chunk, as it is. `given` is the payload's hash when the
/// caller computed it already ([columns_payload_hash]): stated, not computed again.
pub(crate) fn hash_block<'b>(
    h: &mut blake3::Hasher,
    block: &'b [u8],
    given: Option<[u8; 32]>,
) -> Option<(crate::Chunk<'b>, [u8; 32])> {
    let chunk = crate::Block::read_trusted(block)
        .ok()
        .and_then(|b| b.chunk());
    let Some(c) = chunk else {
        h.update(block);
        return None;
    };
    let at = c.payload_offset();
    let end = at.saturating_add(c.payload().len());
    match (block.get(..at), block.get(end..)) {
        (Some(before), Some(after)) => {
            let p = given.unwrap_or_else(|| payload_hash(c.payload()));
            debug_assert_eq!(
                p,
                payload_hash(c.payload()),
                "a payload hash given is the payload's"
            );
            h.update(before);
            h.update(&p);
            h.update(after);
            Some((c, p))
        }
        _ => {
            h.update(block);
            None
        }
    }
}

#[cfg(feature = "alloc")]
/// Bytes a content hash reads at a time where no block reads whole.
const HASH_STEP: usize = 1 << 20;

#[cfg(feature = "alloc")]
/// Bytes of the block magic but its first, which a stretch read at a time keeps back: the magic may start there.
const MAGIC_TAIL: usize = crate::MAGIC.len() - 1;

/// Bytes the blocks a content-hash walk tries and that don't read whole may claim per byte the walk has gone, past
/// [WALK_MISS_BASE_MAX].
///
/// A place is tried only while the misses so far are within this budget, so a stretch of crafted headers, each claiming
/// a long block, costs a bounded multiple of its length, never its square.
pub const WALK_MISS_PER_BYTE_MAX: u64 = 4;

/// Bytes the blocks a content-hash walk tries may claim and miss before it has gone any way (1 MiB).
pub const WALK_MISS_BASE_MAX: u64 = 1 << 20;

/// Feed the first `end` bytes of a piece, read by `read`, to its content hash: each block of [walk_piece] whole
/// ([hash_block]), the bytes between as they are. The hash depends on the bytes alone, not on how a writer or a resume
/// walked them. Whether the walk tries a block at `end`, as [walk_piece] gives it. A resume and a verify walk with
/// [walk_piece] themselves, as they replay each step.
#[cfg(test)]
pub(crate) fn hash_piece<E>(
    h: &mut blake3::Hasher,
    end: u64,
    read: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<bool, E> {
    walk_piece(0, end, read, |step| match step {
        Walked::Block(block) => {
            hash_block(h, block, None);
        }
        Walked::Raw(raw) => {
            h.update(raw);
        }
    })
}

#[cfg(feature = "alloc")]
/// A step of [walk_piece].
pub(crate) enum Walked<'b> {
    /// A block that reads whole.
    Block(&'b [u8]),
    /// Bytes no block explains.
    Raw(&'b [u8]),
}

/// Walk the bytes of a piece from `start` to `end`, read by `read`: at each place a block that reads whole is a step;
/// other bytes are a step up to the next place holding the block magic. A walk from a place other than the start
/// (a resume from a close record) starts with no misses.
///
/// A place is tried as a block only while the lengths of the blocks tried that didn't read whole are within
/// [WALK_MISS_PER_BYTE_MAX] per byte gone plus [WALK_MISS_BASE_MAX]; a block claiming more than `end` holds counts as
/// missed unread. The places tried depend on the bytes before them alone, so a walk of a prefix (a resume) takes the
/// same steps as a walk of the whole piece. Bytes between blocks are read once, in windows of [HASH_STEP].
///
/// Gives whether a walk of more bytes tries a block at `end`: a block written there is a step of its own only then.
/// Past a place whose misses spent the budget, good blocks are bytes no block explains until the walk has gone far
/// enough to try again.
#[cfg(feature = "alloc")]
pub(crate) fn walk_piece<E>(
    start: u64,
    end: u64,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
    mut step: impl FnMut(Walked<'_>),
) -> Result<bool, E> {
    let mut block = Vec::new();
    let mut win = Window::default();
    let mut missed = 0u64;
    let mut at = start;
    let tries = |at: u64, missed: u64| {
        missed
            <= at
                .saturating_mul(WALK_MISS_PER_BYTE_MAX)
                .saturating_add(WALK_MISS_BASE_MAX)
    };
    while let Some(left) = end.checked_sub(at).filter(|n| *n > 0) {
        if tries(at, missed) {
            match try_block(at, left, &mut block, &mut read)? {
                Tried::Block(n) => {
                    step(Walked::Block(&block));
                    at = at.saturating_add(len_u64(n));
                    continue;
                }
                Tried::Missed(n) => missed = missed.saturating_add(len_u64(n)),
                Tried::NoBlock => {}
            }
        }
        let raw = win.raw_at(at, end, &mut read)?;
        step(Walked::Raw(raw));
        at = at.saturating_add(len_u64(raw.len()));
    }
    Ok(tries(at, missed))
}

/// Bytes of a piece [walk_piece] read to step over what no block explains, from `at`.
#[cfg(feature = "alloc")]
#[derive(Default)]
struct Window {
    at: u64,
    bytes: Vec<u8>,
}

#[cfg(feature = "alloc")]
impl Window {
    /// The bytes from `at` (within `end`) up to the next place after it holding the block magic, or as far as one
    /// window goes: never empty. The window is read again only when `at` is past what it can search.
    fn raw_at<E>(
        &mut self,
        at: u64,
        end: u64,
        read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
    ) -> Result<&[u8], E> {
        let reaches_end = |w: &Self| w.at.saturating_add(len_u64(w.bytes.len())) == end;
        let usable = |w: &Self| {
            at.checked_sub(w.at)
                .and_then(|o| usize::try_from(o).ok())
                .filter(|o| match reaches_end(w) {
                    true => *o < w.bytes.len(),
                    false => o.saturating_add(MAGIC_TAIL) < w.bytes.len(),
                })
        };
        let from = match usable(self) {
            Some(o) => o,
            None => {
                let left = end.saturating_sub(at);
                let n = usize::try_from(left).map_or(HASH_STEP, |left| left.min(HASH_STEP));
                self.bytes.resize(n, 0);
                read(at, &mut self.bytes)?;
                self.at = at;
                0
            }
        };
        let rest = self.bytes.get(from..).unwrap_or_default();
        let next = rest
            .get(1..)
            .and_then(|r| {
                r.windows(crate::MAGIC.len())
                    .position(|w| w == crate::MAGIC)
            })
            .map(|k| k.saturating_add(1));
        let take = match next {
            Some(k) => k,
            None if reaches_end(self) => rest.len(),
            None => rest.len().saturating_sub(MAGIC_TAIL).max(1),
        };
        Ok(rest.get(..take).unwrap_or(rest))
    }
}

/// What [try_block] found at a place.
#[cfg(feature = "alloc")]
enum Tried {
    /// A block that reads whole, this long.
    Block(usize),
    /// A header claiming a block this long that doesn't read whole within the bytes walked.
    Missed(usize),
    /// No block header.
    NoBlock,
}

/// Whether a block reads whole at `at` within `left` bytes, read into `buf`.
#[cfg(feature = "alloc")]
fn try_block<E>(
    at: u64,
    left: u64,
    buf: &mut Vec<u8>,
    read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<Tried, E> {
    if left < len_u64(HEADER_LEN) {
        return Ok(Tried::NoBlock);
    }
    let mut header = [0u8; HEADER_LEN];
    read(at, &mut header)?;
    // block_len: at least a header and a CRC, at most MAX_BLOCK_LEN
    let Ok(n) = crate::block_len(&header) else {
        return Ok(Tried::NoBlock);
    };
    if len_u64(n) > left {
        return Ok(Tried::Missed(n));
    }
    buf.resize(n, 0);
    read(at, buf)?;
    Ok(match Block::read(buf).is_ok_and(|b| b.len() == n) {
        true => Tried::Block(n),
        false => Tried::Missed(n),
    })
}

impl PieceId {
    /// No piece id: all zeros.
    pub const NONE: Self = Self([0; 16]);

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }

    /// The id of the piece after one with `id` and `hash`: the first 16 bytes of BLAKE3 in key derivation mode
    /// ("ww_tape piece id") over the hash, then the id. The same chain gives the same ids.
    pub fn after(id: Self, hash: &ContentHash) -> Self {
        Self::derive("ww_tape piece id", &[&hash.0, &id.0])
    }

    /// The id of the first piece of chain 0 of `owner`: [Self::first_of] `(owner, 0)`.
    pub fn first(owner: &OwnerId) -> Self {
        Self::first_of(owner, 0)
    }

    /// The id of the first piece of chain `chain` of `owner` (several chains of one owner, such as one per
    /// source): as [Self::after] over the owner id and the chain number, u64 little endian ("ww_tape first
    /// piece").
    pub fn first_of(owner: &OwnerId, chain: u64) -> Self {
        Self::derive("ww_tape first piece", &[&owner.0, &chain.to_le_bytes()])
    }

    /// The first 16 bytes of BLAKE3 in key derivation mode over `parts`; never all zeros.
    fn derive(context: &str, parts: &[&[u8]]) -> Self {
        let mut h = blake3::Hasher::new_derive_key(context);
        for p in parts {
            h.update(p);
        }
        let mut out = [0u8; 16];
        for (o, b) in out.iter_mut().zip(h.finalize().as_bytes()) {
            *o = *b;
        }
        if out == [0; 16] {
            out[0] = 1;
        }
        Self(out)
    }
}

/// Why a writer closed a piece, as it says: a reader reports it and acts on nothing of it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// The writer was done with the piece; a later session may write on after this close.
    Closed,
    /// The writer stopped on purpose without completing what it wrote (the producer failed); everything written is
    /// good.
    Aborted,
}

impl CloseReason {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Closed),
            2 => Some(Self::Aborted),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Closed => 1,
            Self::Aborted => 2,
        }
    }
}

/// The reason and the next piece's id of a close record or tail; None for a pair no writer makes: a piece goes on in
/// a next one only when closed by a cut, never aborted.
fn ending(reason: u8, next: PieceId) -> Option<(CloseReason, Option<PieceId>)> {
    let reason = CloseReason::from_u8(reason)?;
    let next = (!next.is_none()).then_some(next);
    match (reason, next) {
        (CloseReason::Aborted, Some(_)) => None,
        ending => Some(ending),
    }
}

/// Where a piece sits in its chain, as its head, close and tail say.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct PieceIdent {
    pub id: PieceId,
    pub owner: OwnerId,
    /// 0 for the first piece of a chain, +1 per piece.
    pub seq: u64,
}

/// Where a track's items continue in a piece, from its head.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrackStart {
    pub track_id: TrackId,
    pub track_number: TrackNum,
    /// The first item number of the track in this piece: one past the last in the piece before.
    pub next_item: ItemNum,
    /// No item of the track in this piece starts before this tick: the last start in the piece before.
    pub from_tick: i64,
    /// [TrackDef::owner](crate::TrackDef::owner): with the id, which track this is.
    pub owner: OwnerId,
    /// The track's hash before this piece.
    pub hash: TrackHash,
    /// The hash leaves out units a resume kept unread in a piece before ([TrackRange::incomplete]).
    pub incomplete: bool,
    /// The piece doesn't carry the track's definition: the cut that started it had no room for it in the index
    /// policy's state entries. A reader of the piece without the pieces before names the track as not carried
    /// ([DefinitionsAt::not_carried](crate::reader::DefinitionsAt::not_carried)).
    pub not_carried: bool,
    /// The piece carries every key of this keyed state track live at its start, right after its definition: a
    /// reader of the piece alone finds each of them. Without it, in a piece after another, a key's record in force
    /// may lie only in the pieces before ([KeysAt::not_carried](crate::reader::KeysAt::not_carried)).
    pub keys_carried: bool,
}

/// The items and ticks of one track's chunk blocks in a piece, its counted records and its hash, from its close.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrackRange {
    pub track_id: TrackId,
    pub track_number: TrackNum,
    /// Chunk blocks of the track in the piece; 0 for a track of records alone, whose range fields are 0.
    pub chunks: u32,
    pub first_item: ItemNum,
    pub items: u64,
    pub first_tick: i64,
    pub last_tick: i64,
    pub latest_end: i64,
    /// [TrackDef::owner](crate::TrackDef::owner): with the id, which track this is.
    pub owner: OwnerId,
    /// The track's hash after this piece.
    pub hash: TrackHash,
    /// The track's counted records in the piece.
    pub records: u64,
    /// The track hash leaves out units a resume kept without reading them: the records of a damaged block of the
    /// track's data, a damaged chunk whose index entry holds no unit hash. The hash is still the writer's, but it
    /// isn't that of the track's whole content.
    pub incomplete: bool,
}

impl TrackRange {
    /// Item number past the last.
    pub fn end_item(&self) -> ItemNum {
        self.first_item.saturating_add(self.items)
    }
}

/// The most body bytes the summary block a close names holds ([write_close_summary]).
pub const CLOSE_SUMMARY_MAX: usize = 1 << 20;

/// The summary block a close names ([BlockKind::CloseSummary]) holding `body`, with id `id`.
///
/// It is a header, the caller's body as it gave it, and the CRC; the format reads nothing in the body. None when
/// `body` is longer than [CLOSE_SUMMARY_MAX].
#[cfg(feature = "alloc")]
pub fn write_close_summary(body: &[u8], id: BlockId) -> Option<alloc::vec::Vec<u8>> {
    if body.len() > CLOSE_SUMMARY_MAX {
        return None;
    }
    let len = HEADER_LEN.checked_add(body.len())?.checked_add(CRC_LEN)?;
    let mut b = alloc::vec![0u8; len];
    put_header(&mut b, BlockKind::CloseSummary, len, 0, id)?;
    b.get_mut(HEADER_LEN..len.checked_sub(CRC_LEN)?)?
        .copy_from_slice(body);
    seal(&mut b);
    Some(b)
}

/// The body of the summary block `b` a close names ([write_close_summary]): None for a block of another kind, or a
/// body past [CLOSE_SUMMARY_MAX].
pub fn close_summary_body<'b>(b: &Block<'b>) -> Option<&'b [u8]> {
    if b.kind != BlockKind::CloseSummary {
        return None;
    }
    let body = b
        .bytes
        .get(HEADER_LEN..b.bytes.len().checked_sub(CRC_LEN)?)?;
    (body.len() <= CLOSE_SUMMARY_MAX).then_some(body)
}

/// The fixed part of a piece head.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PieceHead {
    pub ident: PieceIdent,
    /// The previous piece's content hash and id; None for the first piece of a chain.
    pub prev: Option<(ContentHash, PieceId)>,
    /// The session of the block before this piece head on the medium: the session that ended where a writer
    /// appending to a medium found the end of data, or the piece's own session at a cut. [SessionTag::NONE] when
    /// the writer started at the medium's start or can't know. On raw media a reader takes a new session only at a
    /// piece head naming the session just ended, so stale blocks behind the end of data never pass for a new
    /// session.
    pub prev_session: SessionTag,
    /// What the piece's writer computed ([Integrity]): below [Integrity::Full], the hashes it names are absent and
    /// hold zero.
    pub integrity: Integrity,
}

/// A piece's integrity level, the piece head's byte 146: which CRCs and hashes its writer computed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Integrity {
    /// Every CRC and every hash computed.
    Full = 0,
    /// CRCs computed, hashes not: each index entry is marked [ENTRY_UNHASHED](crate::index::ENTRY_UNHASHED).
    CrcOnly = 1,
    /// Payload CRCs and hashes not computed; chunk head CRCs and every structure block stay checked. Each chunk is
    /// marked [FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED), each index entry
    /// [ENTRY_UNHASHED](crate::index::ENTRY_UNHASHED).
    None = 2,
}

impl From<Integrity> for u8 {
    /// The level's byte.
    fn from(level: Integrity) -> Self {
        match level {
            Integrity::Full => 0,
            Integrity::CrcOnly => 1,
            Integrity::None => 2,
        }
    }
}

impl TryFrom<u8> for Integrity {
    type Error = IndexError;

    /// The level of byte `level`; [IndexError::UnknownIntegrity] for a value no level has.
    fn try_from(level: u8) -> Result<Self, IndexError> {
        match level {
            0 => Ok(Self::Full),
            1 => Ok(Self::CrcOnly),
            2 => Ok(Self::None),
            level => Err(IndexError::UnknownIntegrity { level }),
        }
    }
}

/// Bytes of a block of `fixed` bytes of body and `tracks` rows of `row` bytes; `usize::MAX`, longer than any block,
/// when that overflows.
const fn block_len_of(fixed: usize, tracks: usize, row: usize) -> usize {
    HEADER_LEN
        .saturating_add(fixed)
        .saturating_add(tracks.saturating_mul(row))
        .saturating_add(CRC_LEN)
}

/// Bytes of a piece head block with `tracks`.
pub const fn piece_head_len(tracks: usize) -> usize {
    block_len_of(HEAD_FIXED_LEN, tracks, HEAD_TRACK_LEN)
}

/// The first `len` bytes of `buf` for a block of `tracks` rows: at most a block's length, `buf` long enough.
fn block_buf(buf: &mut [u8], len: usize, tracks: usize) -> Result<(&mut [u8], u32), IndexError> {
    let count = u32::try_from(tracks)
        .ok()
        .filter(|_| len <= crate::MAX_BLOCK_LEN)
        .ok_or(IndexError::Invalid("longer than a block"))?;
    let b = buf.get_mut(..len).ok_or(IndexError::Full { needed: len })?;
    Ok((b, count))
}

/// Write a piece head block into `buf`; returns its length.
pub fn write_piece_head(
    buf: &mut [u8],
    id: BlockId,
    head: &PieceHead,
    tracks: &[TrackStart],
) -> Result<usize, IndexError> {
    let len = piece_head_len(tracks.len());
    let (b, count) = block_buf(buf, len, tracks.len())?;
    put_header(b, BlockKind::PieceHead, len, count, id)
        .ok_or(IndexError::Invalid("longer than a block"))?;
    fill_piece_head(b, head, tracks).ok_or(IndexError::Invalid("piece head layout"))?;
    seal(b);
    Ok(len)
}

/// The body of the piece head block `b`, sized for `tracks`.
fn fill_piece_head(b: &mut [u8], head: &PieceHead, tracks: &[TrackStart]) -> Option<()> {
    b.get_mut(HEADER_LEN..)?.fill(0);
    put_ident(b, 32, 48, 80, &head.ident)?;
    put_u32(b, 92, head.prev_session.get())?;
    if let Some((hash, id)) = head.prev {
        put_u32(b, 88, FLAG_HAS_PREV)?;
        put(b, 96, hash.0)?;
        put(b, 128, id.0)?;
    }
    put_u16(b, HEAD_ROW_LEN_AT, u16::try_from(HEAD_TRACK_LEN).ok()?)?;
    *b.get_mut(HEAD_INTEGRITY_AT)? = u8::from(head.integrity);
    let rows = b
        .get_mut(HEAD_TRACKS_AT..)?
        .as_chunks_mut::<HEAD_TRACK_LEN>()
        .0
        .iter_mut();
    for (row, t) in rows.zip(tracks) {
        put_u64(row, 0, t.track_id.get())?;
        put_u32(row, 8, t.track_number.get())?;
        let flags = row_flags(t.incomplete)
            | if t.not_carried { ROW_NOT_CARRIED } else { 0 }
            | if t.keys_carried { ROW_KEYS_CARRIED } else { 0 };
        put_u32(row, HEAD_ROW_FLAGS_AT, flags)?;
        put_u64(row, 16, t.next_item.get())?;
        put_i64(row, 24, t.from_tick)?;
        put(row, 32, t.owner.0)?;
        put(row, 64, t.hash.0)?;
    }
    Some(())
}

fn put_ident(
    b: &mut [u8],
    id_at: usize,
    owner_at: usize,
    seq_at: usize,
    ident: &PieceIdent,
) -> Option<()> {
    put(b, id_at, ident.id.0)?;
    put(b, owner_at, ident.owner.0)?;
    put_u64(b, seq_at, ident.seq)
}

fn get_ident(b: &[u8], id_at: usize, owner_at: usize, seq_at: usize) -> Option<PieceIdent> {
    Some(PieceIdent {
        id: PieceId(array(b, id_at)?),
        owner: OwnerId(array(b, owner_at)?),
        seq: le_u64(b, seq_at)?,
    })
}

/// Check a good block is of `kind`.
fn of_kind(block: &Block<'_>, kind: BlockKind) -> Result<(), IndexError> {
    if block.kind != kind || !block.body_known() {
        return Err(IndexError::Kind);
    }
    Ok(())
}

/// Check a good block is of `kind` and holds `fixed` bytes of body, then rows for each of its count, each as long as
/// the u16 at `row_len_at` says and at least `row_min`, then `padding` bytes; returns the rows and their length.
fn body<'b>(
    block: &Block<'b>,
    kind: BlockKind,
    fixed: usize,
    (row_len_at, row_min): (usize, usize),
    padding: usize,
) -> Result<(&'b [u8], usize), IndexError> {
    of_kind(block, kind)?;
    let row = le_u16(block.bytes(), row_len_at)
        .map(usize::from)
        .filter(|r| *r >= row_min)
        .ok_or(IndexError::BadHead)?;
    let n = usize::try_from(block.record_count).ok();
    if n.map(|n| block_len_of(fixed, n, row).saturating_add(padding)) != Some(block.len()) {
        return Err(IndexError::BadHead);
    }
    let at = HEADER_LEN.checked_add(fixed);
    let end = block
        .len()
        .checked_sub(CRC_LEN)
        .and_then(|e| e.checked_sub(padding));
    at.zip(end)
        .and_then(|(at, end)| block.bytes().get(at..end))
        .map(|rows| (rows, row))
        .ok_or(IndexError::BadHead)
}

/// A track row's flags word.
fn row_flags(incomplete: bool) -> u32 {
    if incomplete { ROW_INCOMPLETE } else { 0 }
}

/// Refuse rows of `row` bytes each (at least the fixed row length, so each holds the u32 at `at`) whose flags at
/// `at` hold a bit past `known`.
fn known_row_flags(rows: &[u8], row: usize, at: usize, known: u32) -> Result<(), IndexError> {
    if rows
        .chunks_exact(row)
        .any(|r| le_u32(r, at).is_none_or(|f| f & !known != 0))
    {
        return Err(IndexError::Invalid(
            "a track row flag this build doesn't know",
        ));
    }
    Ok(())
}

/// A piece head block read in place.
#[derive(Copy, Clone, Debug)]
pub struct PieceHeadBlock<'b> {
    head: PieceHead,
    /// The tracks, `row` bytes each
    rows: &'b [u8],
    /// At least [HEAD_TRACK_LEN]
    row: usize,
}

impl<'b> PieceHeadBlock<'b> {
    /// The piece head of a good block.
    pub fn read(block: &Block<'b>) -> Result<Self, IndexError> {
        let (rows, row) = body(
            block,
            BlockKind::PieceHead,
            HEAD_FIXED_LEN,
            (HEAD_ROW_LEN_AT, HEAD_TRACK_LEN),
            0,
        )?;
        let integrity = level_of(block)?;
        known_row_flags(
            rows,
            row,
            HEAD_ROW_FLAGS_AT,
            ROW_INCOMPLETE | ROW_NOT_CARRIED | ROW_KEYS_CARRIED,
        )?;
        let link = read_link(block.bytes()).ok_or(IndexError::BadHead)?;
        let head = PieceHead {
            ident: link.ident,
            prev: link.prev,
            prev_session: link.prev_session,
            integrity,
        };
        Ok(PieceHeadBlock { head, rows, row })
    }

    #[cfg(feature = "alloc")]
    /// The links of a good piece head block whose integrity level this build doesn't know ([Self::read]'s
    /// [IndexError::UnknownIntegrity]), with that error: what a chain check and a
    /// raw medium's walk need of a piece they refuse to read. None for a block that is no such head.
    pub(crate) fn refused(block: &Block<'_>) -> Option<(HeadLink, IndexError)> {
        body(
            block,
            BlockKind::PieceHead,
            HEAD_FIXED_LEN,
            (HEAD_ROW_LEN_AT, HEAD_TRACK_LEN),
            0,
        )
        .ok()?;
        let error = level_of(block).err()?;
        Some((read_link(block.bytes())?, error))
    }

    /// The integrity byte of a good piece head block whose level this build refuses ([Self::refused]); None for a
    /// block that is no such head.
    #[cfg(feature = "std")]
    pub(crate) fn refused_level(block: &Block<'_>) -> Option<u8> {
        Self::refused(block)?;
        le_u8(block.bytes(), HEAD_INTEGRITY_AT)
    }

    pub fn head(&self) -> PieceHead {
        self.head
    }

    pub fn tracks(&self) -> impl Iterator<Item = TrackStart> + '_ {
        self.rows.chunks_exact(self.row).map_while(|row| {
            Some(TrackStart {
                track_id: TrackId::new(le_u64(row, 0)?),
                track_number: TrackNum::try_new(le_u32(row, 8)?)?,
                next_item: ItemNum::new(le_u64(row, 16)?),
                from_tick: le_i64(row, 24)?,
                owner: OwnerId(array(row, 32)?),
                hash: TrackHash(array(row, 64)?),
                incomplete: le_u32(row, HEAD_ROW_FLAGS_AT)? & ROW_INCOMPLETE != 0,
                not_carried: le_u32(row, HEAD_ROW_FLAGS_AT)? & ROW_NOT_CARRIED != 0,
                keys_carried: le_u32(row, HEAD_ROW_FLAGS_AT)? & ROW_KEYS_CARRIED != 0,
            })
        })
    }
}

/// The integrity level of the piece head block `block`, its body read already.
fn level_of(block: &Block<'_>) -> Result<Integrity, IndexError> {
    let level = le_u8(block.bytes(), HEAD_INTEGRITY_AT).ok_or(IndexError::BadHead)?;
    Integrity::try_from(level)
}

/// What a piece head says of the piece's place in its chain, whatever its integrity level.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadLink {
    pub(crate) ident: PieceIdent,
    pub(crate) prev: Option<(ContentHash, PieceId)>,
    pub(crate) prev_session: SessionTag,
}

fn read_link(b: &[u8]) -> Option<HeadLink> {
    let prev = if le_u32(b, 88)? & FLAG_HAS_PREV != 0 {
        Some((ContentHash(array(b, 96)?), PieceId(array(b, 128)?)))
    } else {
        None
    };
    Some(HeadLink {
        ident: get_ident(b, 32, 48, 80)?,
        prev,
        prev_session: SessionTag::from_u32(le_u32(b, 92)?),
    })
}

/// The fixed part of a close record.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Close {
    pub reason: CloseReason,
    pub hash: ContentHash,
    /// The newest checkpoint index block.
    pub checkpoint: Option<u64>,
    /// The last final index block (each points at the one before, across closes).
    pub final_index: Option<u64>,
    /// The previous close record's offset in the piece; None for the piece's first.
    pub prev: Option<u64>,
    /// Where the summary block of the caller's own ([write_close_summary]) placed right before the final index is, and
    /// its length; None for none.
    pub summary: Option<(u64, u32)>,
    /// The piece's ident; None for a file written without a piece head.
    pub ident: Option<PieceIdent>,
    /// The next piece's id, chosen at the cut: some when the piece goes on in the next one, the link of its chain.
    pub next: Option<PieceId>,
}

/// A piece of the chain removed on purpose, as a close record names it ([Close], [check_chain]).
///
/// It gives the piece's sequence number, its id and the content hash of its close in force. A reader links over the
/// removed pieces by the id the piece before them names and the hash and id the piece after them names.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemovedPiece {
    pub seq: u64,
    pub id: PieceId,
    /// The content hash of the piece's close in force: what the next piece's head names.
    pub hash: ContentHash,
}

/// Bytes of a close record with `tracks` and `removed` removed-piece rows, without padding.
pub const fn close_len(tracks: usize, removed: usize) -> usize {
    block_len_of(CLOSE_FIXED_LEN, tracks, CLOSE_TRACK_LEN)
        .saturating_add(removed.saturating_mul(REMOVED_ROW_LEN))
}

/// Zero bytes a close record is padded with, below [PIECE_ALIGN].
///
/// The close has `tracks` track rows and `removed` removed-piece rows and sits at `close_offset` in its piece; the
/// padding ends the piece, after its tail, on a multiple of [PIECE_ALIGN].
pub const fn close_padding(close_offset: u64, tracks: usize, removed: usize) -> usize {
    // only the low bits count: the end modulo 2^64 has them
    let end = close_offset
        .wrapping_add(len_u64(close_len(tracks, removed)))
        .wrapping_add(len_u64(TAIL_LEN));
    to_boundary(end, PIECE_ALIGN.wrapping_sub(1))
}

fn opt(v: Option<u64>) -> u64 {
    v.unwrap_or(NONE)
}

fn get_opt(b: &[u8], at: usize) -> Option<Option<u64>> {
    le_u64(b, at).map(|v| Some(v).filter(|v| *v != NONE))
}

/// Write a close record into `buf`; returns its length.
///
/// The record names the pieces `removed` and has `padding` zero bytes after its rows; `buf` is at least [close_len]
/// plus `padding` long. A padding of [PIECE_ALIGN] or more is refused, and so are removed pieces past
/// [REMOVED_PIECES_MAX] or not in ascending order of distinct sequence numbers.
pub fn write_close(
    buf: &mut [u8],
    id: BlockId,
    close: &Close,
    (tracks, removed): (&[TrackRange], &[RemovedPiece]),
    padding: usize,
) -> Result<usize, IndexError> {
    let pad = u32::try_from(padding)
        .ok()
        .filter(|p| *p < PIECE_ALIGN)
        .ok_or(IndexError::Invalid("close padding of 4 KiB or more"))?;
    if removed.len() > REMOVED_PIECES_MAX {
        return Err(IndexError::Invalid(
            "more removed pieces than REMOVED_PIECES_MAX",
        ));
    }
    if !removed.is_sorted_by(|a, b| a.seq < b.seq) {
        return Err(IndexError::Invalid(
            "removed pieces not in ascending order of distinct seqs",
        ));
    }
    let len = close_len(tracks.len(), removed.len()).saturating_add(padding);
    let (b, count) = block_buf(buf, len, tracks.len())?;
    put_header(b, BlockKind::Close, len, count, id)
        .ok_or(IndexError::Invalid("longer than a block"))?;
    fill_close(b, close, tracks).ok_or(IndexError::Invalid("close layout"))?;
    fill_removed(b, tracks.len(), removed).ok_or(IndexError::Invalid("close layout"))?;
    put_u32(b, 172, pad).ok_or(IndexError::Invalid("close layout"))?;
    seal(b);
    Ok(len)
}

/// The removed-piece rows of the close record `b` of `tracks` track rows, and their count and row length.
fn fill_removed(b: &mut [u8], tracks: usize, removed: &[RemovedPiece]) -> Option<()> {
    put_u16(b, REMOVED_ROW_LEN_AT, u16::try_from(REMOVED_ROW_LEN).ok()?)?;
    put_u32(b, REMOVED_COUNT_AT, u32::try_from(removed.len()).ok()?)?;
    let at = CLOSE_TRACKS_AT.checked_add(tracks.checked_mul(CLOSE_TRACK_LEN)?)?;
    let rows = b
        .get_mut(at..)?
        .as_chunks_mut::<REMOVED_ROW_LEN>()
        .0
        .iter_mut();
    for (row, r) in rows.zip(removed) {
        put_u64(row, 0, r.seq)?;
        put(row, 8, r.id.0)?;
        put(row, 24, r.hash.0)?;
    }
    Some(())
}

/// The body of the close record `b`, sized for `tracks`.
fn fill_close(b: &mut [u8], close: &Close, tracks: &[TrackRange]) -> Option<()> {
    b.get_mut(HEADER_LEN..)?.fill(0);
    put(b, 32, [close.reason.to_u8()])?;
    put(b, 40, close.hash.0)?;
    put_u64(b, 72, opt(close.checkpoint))?;
    put_u64(b, 80, opt(close.final_index))?;
    put_u64(b, 88, opt(close.prev))?;
    let (summary_at, summary_len) = close.summary.unwrap_or((NONE, 0));
    put_u64(b, 176, summary_at)?;
    put_u32(b, 184, summary_len)?;
    if let Some(ident) = &close.ident {
        put_ident(b, 96, 128, 160, ident)?;
        put_u32(b, 168, u32::from(FLAG_PIECE))?;
    }
    put(b, 112, close.next.unwrap_or(PieceId::NONE).0)?;
    put_u16(b, CLOSE_ROW_LEN_AT, u16::try_from(CLOSE_TRACK_LEN).ok()?)?;
    let rows = b
        .get_mut(CLOSE_TRACKS_AT..)?
        .as_chunks_mut::<CLOSE_TRACK_LEN>()
        .0
        .iter_mut();
    for (row, t) in rows.zip(tracks) {
        put_u64(row, 0, t.track_id.get())?;
        put_u32(row, 8, t.track_number.get())?;
        put_u32(row, 12, t.chunks)?;
        put_u64(row, 16, t.first_item.get())?;
        put_u64(row, 24, t.items)?;
        put_i64(row, 32, t.first_tick)?;
        put_i64(row, 40, t.last_tick)?;
        put_i64(row, 48, t.latest_end)?;
        put(row, 56, t.owner.0)?;
        put(row, 88, t.hash.0)?;
        put_u64(row, 120, t.records)?;
        put_u32(row, ROW_FLAGS_AT, row_flags(t.incomplete))?;
    }
    Some(())
}

/// A close record read in place.
#[derive(Copy, Clone, Debug)]
pub struct CloseBlock<'b> {
    close: Close,
    /// The tracks, `row` bytes each
    rows: &'b [u8],
    /// At least [CLOSE_TRACK_LEN]
    row: usize,
    /// The removed pieces, `removed_row` bytes each, ascending distinct seqs
    removed: &'b [u8],
    /// At least [REMOVED_ROW_LEN]
    removed_row: usize,
}

impl<'b> CloseBlock<'b> {
    /// The close of a good block.
    pub fn read(block: &Block<'b>) -> Result<Self, IndexError> {
        of_kind(block, BlockKind::Close)?;
        let b = block.bytes();
        let padding = le_u32(b, 172)
            .filter(|p| *p < PIECE_ALIGN)
            .map(u32_len)
            .ok_or(IndexError::BadHead)?;
        let removed_n = le_u32(b, REMOVED_COUNT_AT)
            .map(u32_len)
            .filter(|n| *n <= REMOVED_PIECES_MAX)
            .ok_or(IndexError::Invalid(
                "more removed pieces than REMOVED_PIECES_MAX",
            ))?;
        // a close without removed pieces may state no row length: closes written before rows existed hold 0 there
        let removed_row = le_u16(b, REMOVED_ROW_LEN_AT)
            .map(usize::from)
            .map(|r| {
                if removed_n == 0 {
                    r.max(REMOVED_ROW_LEN)
                } else {
                    r
                }
            })
            .filter(|r| *r >= REMOVED_ROW_LEN)
            .ok_or(IndexError::BadHead)?;
        let removed_len = removed_n
            .checked_mul(removed_row)
            .ok_or(IndexError::BadHead)?;
        let (all, row) = body(
            block,
            BlockKind::Close,
            CLOSE_FIXED_LEN,
            (CLOSE_ROW_LEN_AT, CLOSE_TRACK_LEN),
            padding
                .checked_add(removed_len)
                .ok_or(IndexError::BadHead)?,
        )?;
        let rows_end = all.len();
        let removed = b
            .get(CLOSE_TRACKS_AT.saturating_add(rows_end)..)
            .and_then(|r| r.get(..removed_len))
            .ok_or(IndexError::BadHead)?;
        let close = read_close(b).ok_or(IndexError::BadHead)?;
        known_row_flags(all, row, ROW_FLAGS_AT, ROW_INCOMPLETE)?;
        let c = CloseBlock {
            close,
            rows: all,
            row,
            removed,
            removed_row,
        };
        let mut seqs = c.removed().map(|r| r.seq);
        let ascending = seqs
            .next()
            .is_none_or(|first| seqs.try_fold(first, |a, b| (a < b).then_some(b)).is_some());
        if !ascending {
            return Err(IndexError::Invalid(
                "removed pieces not in ascending order of distinct seqs",
            ));
        }
        Ok(c)
    }

    pub fn close(&self) -> Close {
        self.close
    }

    /// The pieces of the chain the close names as removed, ascending by sequence number.
    pub fn removed(&self) -> impl Iterator<Item = RemovedPiece> + '_ {
        self.removed
            .chunks_exact(self.removed_row)
            .map_while(|row| {
                Some(RemovedPiece {
                    seq: le_u64(row, 0)?,
                    id: PieceId(array(row, 8)?),
                    hash: ContentHash(array(row, 24)?),
                })
            })
    }

    pub fn tracks(&self) -> impl Iterator<Item = TrackRange> + '_ {
        self.rows.chunks_exact(self.row).map_while(|row| {
            Some(TrackRange {
                track_id: TrackId::new(le_u64(row, 0)?),
                track_number: TrackNum::try_new(le_u32(row, 8)?)?,
                chunks: le_u32(row, 12)?,
                first_item: ItemNum::new(le_u64(row, 16)?),
                items: le_u64(row, 24)?,
                first_tick: le_i64(row, 32)?,
                last_tick: le_i64(row, 40)?,
                latest_end: le_i64(row, 48)?,
                owner: OwnerId(array(row, 56)?),
                hash: TrackHash(array(row, 88)?),
                records: le_u64(row, 120)?,
                incomplete: le_u32(row, ROW_FLAGS_AT)? & ROW_INCOMPLETE != 0,
            })
        })
    }
}

/// The fixed part of the close record `b`; None when its reason is none this reader knows, or an aborted piece names
/// a next one.
fn read_close(b: &[u8]) -> Option<Close> {
    let piece = le_u32(b, 168)? & u32::from(FLAG_PIECE) != 0;
    let (reason, next) = ending(le_u8(b, 32)?, PieceId(array(b, 112)?))?;
    let ident = if piece {
        Some(get_ident(b, 96, 128, 160)?)
    } else {
        None
    };
    Some(Close {
        reason,
        hash: ContentHash(array(b, 40)?),
        checkpoint: get_opt(b, 72)?,
        final_index: get_opt(b, 80)?,
        prev: get_opt(b, 88)?,
        summary: get_opt(b, 176)?.zip(Some(le_u32(b, 184)?)),
        ident,
        next,
    })
}

/// What a tail block says.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tail {
    /// Bytes from the piece's start to the end of the tail.
    pub piece_len: u64,
    pub close_offset: u64,
    pub close_len: u32,
    pub reason: CloseReason,
    pub hash: ContentHash,
    pub ident: Option<PieceIdent>,
    pub next: Option<PieceId>,
}

impl Tail {
    /// Whether this tail ends the piece after `close`, the close record at `close_at` in the piece: it names that
    /// record, states what the record states, and lies right after it as the piece's last bytes.
    pub fn ends(&self, close: &Close, close_at: u64) -> bool {
        let tail_at = close_at.checked_add(u64::from(self.close_len));
        self.close_offset == close_at
            && u64::try_from(TAIL_LEN)
                .is_ok_and(|n| tail_at.and_then(|at| at.checked_add(n)) == Some(self.piece_len))
            && self.hash == close.hash
            && self.reason == close.reason
            && self.next == close.next
            && self.ident == close.ident
    }

    /// Write the tail block into `buf` (at least [TAIL_LEN] bytes); returns [TAIL_LEN].
    ///
    /// # Panics
    ///
    /// When `buf` is shorter than [TAIL_LEN].
    pub fn write(&self, buf: &mut [u8], id: BlockId) -> usize {
        let written = buf.get_mut(..TAIL_LEN).and_then(|b| {
            put_header(b, BlockKind::Tail, TAIL_LEN, 0, id)?;
            self.fill(b)?;
            seal(b);
            Some(())
        });
        assert!(written.is_some(), "a tail is {TAIL_LEN} bytes");
        TAIL_LEN
    }

    /// The body of the tail block `b`, [TAIL_LEN] bytes.
    fn fill(&self, b: &mut [u8]) -> Option<()> {
        b.get_mut(HEADER_LEN..)?.fill(0);
        put_u64(b, 32, self.piece_len)?;
        put_u64(b, 40, self.close_offset)?;
        put_u32(b, 48, self.close_len)?;
        put(b, 52, [self.reason.to_u8()])?;
        put(b, 56, self.hash.0)?;
        if let Some(ident) = &self.ident {
            put(b, 53, [FLAG_PIECE])?;
            put_ident(b, 88, 120, 152, ident)?;
        }
        put(b, 104, self.next.unwrap_or(PieceId::NONE).0)
    }

    /// Read a tail from its [TAIL_LEN] bytes (a file's last ones), CRC checked.
    pub fn read(bytes: &[u8]) -> Result<Self, IndexError> {
        let block = Block::read(bytes)?;
        if block.kind != BlockKind::Tail || !block.body_known() {
            return Err(IndexError::Kind);
        }
        let b = block.bytes();
        if b.len() != TAIL_LEN || block.record_count != 0 {
            return Err(IndexError::BadHead);
        }
        Self::read_body(b).ok_or(IndexError::BadHead)
    }

    /// The tail in the good tail block `b`; None when its reason is unknown, an aborted piece names a next one, or
    /// its offsets don't fit the piece.
    fn read_body(b: &[u8]) -> Option<Self> {
        let (reason, next) = ending(le_u8(b, 52)?, PieceId(array(b, 104)?))?;
        let piece_len = le_u64(b, 32)?;
        let close_offset = le_u64(b, 40)?;
        let close_len = le_u32(b, 48)?;
        let fits = close_offset
            .checked_add(u64::from(close_len))?
            .checked_add(u64::try_from(TAIL_LEN).ok()?)?
            <= piece_len;
        if !fits {
            return None;
        }
        let ident = if le_u8(b, 53)? & FLAG_PIECE != 0 {
            Some(get_ident(b, 88, 120, 152)?)
        } else {
            None
        };
        Some(Self {
            piece_len,
            close_offset,
            close_len,
            reason,
            hash: ContentHash(array(b, 56)?),
            ident,
            next,
        })
    }
}

/// Bytes of an append head block.
pub const APPEND_HEAD_LEN: usize = 84;

/// What an append head says: the close record and tail a writer wrote on after, and that close's hash, which the
/// next close's hash chains over (see the [module docs](self)).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AppendHead {
    /// The close record's offset in the piece.
    pub close_offset: u64,
    /// Its tail's offset in the piece.
    pub tail_offset: u64,
    /// Its content hash.
    pub hash: ContentHash,
}

impl AppendHead {
    /// Write the append head block into `buf` (at least [APPEND_HEAD_LEN] bytes); returns [APPEND_HEAD_LEN].
    ///
    /// # Panics
    ///
    /// When `buf` is shorter than [APPEND_HEAD_LEN].
    pub fn write(&self, buf: &mut [u8], id: BlockId) -> usize {
        let written = buf.get_mut(..APPEND_HEAD_LEN).and_then(|b| {
            put_header(b, BlockKind::AppendHead, APPEND_HEAD_LEN, 0, id)?;
            b.get_mut(HEADER_LEN..)?.fill(0);
            put_u64(b, 32, self.close_offset)?;
            put_u64(b, 40, self.tail_offset)?;
            put(b, 48, self.hash.0)?;
            seal(b);
            Some(())
        });
        assert!(
            written.is_some(),
            "an append head is {APPEND_HEAD_LEN} bytes"
        );
        APPEND_HEAD_LEN
    }

    /// The append head of a good block.
    pub fn read(block: &Block<'_>) -> Result<Self, IndexError> {
        of_kind(block, BlockKind::AppendHead)?;
        let b = block.bytes();
        if b.len() != APPEND_HEAD_LEN || block.record_count != 0 {
            return Err(IndexError::BadHead);
        }
        let head = (|| {
            Some(Self {
                close_offset: le_u64(b, 32)?,
                tail_offset: le_u64(b, 40)?,
                hash: ContentHash(array(b, 48)?),
            })
        })();
        head.ok_or(IndexError::BadHead)
    }

    /// Whether this head goes on from the close record at `close_offset` stating `hash`, whose tail is at
    /// `tail_offset`.
    pub fn follows(&self, close_offset: u64, tail_offset: u64, hash: &ContentHash) -> bool {
        self.close_offset == close_offset && self.tail_offset == tail_offset && self.hash == *hash
    }
}

/// The content hash a close record after the one stating `prev` chains over: BLAKE3 fed `prev`, then the bytes from
/// that close record on ([hash_block] for each block).
#[cfg(feature = "alloc")]
pub(crate) fn chained(prev: &ContentHash) -> blake3::Hasher {
    let mut h = blake3::Hasher::new();
    h.update(&prev.0);
    h
}

#[cfg(feature = "alloc")]
mod chain;
#[cfg(feature = "alloc")]
pub(crate) use chain::Removed;
#[cfg(feature = "alloc")]
pub use chain::{ChainEnd, ChainLink, ChainReport, HeadStatus, PieceSummary, check_chain};

#[cfg(feature = "alloc")]
pub(crate) mod writer;
#[cfg(feature = "alloc")]
pub use writer::{
    CUT_SLACK, ChainOptions, ChainPiece, ChunkPlaced, CutPolicy, PieceChain, PieceTapes,
};

#[cfg(test)]
mod tests;
