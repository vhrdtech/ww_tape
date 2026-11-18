//! Tape pieces: one recording cut into complete files, and how a piece ends.
//!
//! A **piece** is a complete tape file: a [piece head](PieceHead) block, blocks of definitions, chunk, summary and
//! record blocks with checkpoint [index](crate::index) blocks between them, then at the finish a final index, a
//! [finish](Finish) block and a fixed-size [tail](Tail) block. Any piece reads alone: a writer puts the definitions
//! each block of data needs close behind it in the same piece ([state](crate::state)); pieces written back to back are one valid tape. A file a writer started without a piece head is one piece
//! too, with no ids.
//!
//! A writer cuts a recording into pieces at chunk boundaries ([PieceChain]). The head names the previous piece by its
//! content hash and its id; the previous piece's tail names this piece's id, derived at the cut from the previous
//! piece's id and the content hash in its finish ([PieceId::after]), so the chain is checked both ways ([check_chain]).
//! Each piece carries the owner id, its sequence number in the chain and, per track, where its items continue (head)
//! and the items and ticks it holds (finish). A chain whose first piece is gone reads as head cut, not as damage.
//!
//! The finish block is the last thing a writer places before the tail. Its content hash is BLAKE3 over the piece's
//! bytes before it; a piece without one is live or was cut off, and reads up to its last good block. The tail sits
//! at the very end of the file, [TAIL_LEN] bytes, so a reader finds it with one read and from it the finish, the
//! indexes and the start of the piece; the tail of the piece before is just before that start.
//!
//! A finished piece is a multiple of [PIECE_ALIGN] (4 KiB) long: the writer pads the finish block's body with zeros
//! ([finish_padding]), so the tail, still the piece's last bytes, ends on the boundary. Pieces then clone back to back
//! (reflinks on btrfs or XFS) with nothing between them. Only the end is aligned; the blocks inside stay where they
//! are, and a piece without a finish ends where it ends.
//!
//! Layouts after the 32-byte block header (the header's count: tracks), little endian:
//!
//! ```text
//! piece head (kind 10)
//!  32 [16] piece id                 48 [32] owner id                80 u64 sequence number in the chain
//!  88 u32  flags: bit 0 has a previous piece                        92 u32 previous session tag (0: none or unknown)
//!  96 [32] previous piece's content hash                           128 [16] previous piece's id
//! 144 u16  track row length, 96 (a reader takes longer rows and reads their first 96 bytes)    146 [6] reserved
//! 152 ...  tracks × 96, one per track carried over from the pieces before:
//!            0 u64 id    8 u32 number    12 u32 reserved    16 u64 next item (the first item number this piece holds)
//!           24 i64 from tick (no item of the track here starts before it)
//!           32 [32] owner (TrackDef::owner; zeros: the piece's owner)
//!           64 [32] track hash before this piece ([TrackHash])
//!
//! finish (kind 8)
//!  32 u8   state (1 complete, 2 continued in the next piece, 3 aborted)    33 u8 reserved
//!  34 u16  track row length, 128 (a reader takes longer rows and reads their first 128 bytes)    36 [4] reserved
//!  40 [32] content hash: BLAKE3 of the piece's bytes before this block, each chunk payload by its hash (below)
//!  72 u64  newest checkpoint index offset    80 u64 last final index offset    88 u64 reserved, u64::MAX
//!          (u64::MAX for none)
//!  96 [16] piece id    112 [16] next piece id    128 [32] owner id    160 u64 sequence number
//! 168 u32  flags: bit 0 the piece has a head    172 u32 padding: zero bytes after the tracks, below [PIECE_ALIGN]
//! 176 ...  tracks × 128, one per track with a counted chunk or record in the piece, ascending id then owner:
//!            0 u64 id    8 u32 number    12 u32 chunks (0: records only, the range fields below 0)
//!           16 u64 first item    24 u64 items    32 i64 first tick    40 i64 last tick
//!           48 i64 latest end (of the chunk blocks in the piece)
//!           56 [32] owner (TrackDef::owner)
//!           88 [32] track hash after this piece ([TrackHash])
//!          120 u64 records: the track's counted records in the piece
//!     ...  padding zeros, covered by the CRC
//!
//! tail (kind 9), TAIL_LEN bytes, the last of the piece
//!  32 u64  piece length, from the piece's start to the end of this block
//!  40 u64  finish offset    48 u32 finish length    52 u8 state    53 u8 flags (as the finish's)    54 u16 reserved
//!  56 [32] content hash    88 [16] piece id    104 [16] next piece id    120 [32] owner id    152 u64 sequence
//! 160 u32  CRC
//! ```
//!
//! Offsets are from the start of the piece. Ids are opaque to `ww_tape`, their meaning the application's: a piece id
//! is 16 bytes, an owner id 32 (e.g. a hash the producer derives); all zeros is none. A track is told apart by its id
//! and its owner ([TrackDef::owner](crate::TrackDef::owner)): two owners' tracks of one identity share a file, each
//! with its own number, item numbers and hash.
//!
//! **Content hash.** BLAKE3 over the piece's bytes before its finish block, in order, except that the payload of
//! every chunk block that reads whole (from its payload offset to its CRC: the columns and their 16 zero bytes) is
//! replaced by the 32-byte BLAKE3 hash of that payload. Every byte is covered; the payload is hashed once, for this
//! hash and the track hash both.
//!
//! **Track hash.** Each track has a running hash ([TrackHash]) over its content as written, in the order written:
//! 32 zero bytes before its first, then `h = BLAKE3(h || unit)` for each unit:
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

use crate::bytes::{
    array, le_i64, le_u8, le_u16, le_u32, le_u64, put, put_i64, put_u16, put_u32, put_u64,
};
use crate::chunk::{len_u64, to_boundary, u32_len};
use crate::index::{IndexError, NONE, seal};
use crate::{Block, BlockId, BlockKind, CRC_LEN, HEADER_LEN, SessionTag, put_header};

/// Bytes of a tail block.
pub const TAIL_LEN: usize = 164;
/// A finished piece's length is a multiple of this: 4 KiB, [crate::ALIGNMENT].
pub const PIECE_ALIGN: u32 = crate::ALIGNMENT;
const _: () = assert!(PIECE_ALIGN.is_power_of_two(), "padding takes the low bits");
/// Bytes of the fixed part of a piece head body.
const HEAD_FIXED_LEN: usize = 120;
/// Bytes of a piece head's track row this crate writes; a reader takes longer ones.
const HEAD_TRACK_LEN: usize = 96;
/// Where a piece head names its row length.
const HEAD_ROW_LEN_AT: usize = 144;
/// Where a piece head's tracks start.
const HEAD_TRACKS_AT: usize = HEADER_LEN + HEAD_FIXED_LEN;
/// Bytes of the fixed part of a finish body.
const FINISH_FIXED_LEN: usize = 144;
/// Bytes of a finish's track row this crate writes; a reader takes longer ones.
const FINISH_TRACK_LEN: usize = 128;
/// Where a finish names its row length.
const FINISH_ROW_LEN_AT: usize = 34;
/// Where a finish block's tracks start.
const FINISH_TRACKS_AT: usize = HEADER_LEN + FINISH_FIXED_LEN;

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
    pub const NONE: OwnerId = OwnerId([0; 32]);

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }
}

/// BLAKE3 of a piece's bytes before its finish block, each chunk payload by its hash (see the [module docs](self)).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ContentHash(pub [u8; 32]);

/// A track's running content hash, see the [module docs](self#track-hash).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TrackHash(pub [u8; 32]);

/// A unit of a chunk in [TrackHash].
const UNIT_CHUNK: u8 = 1;
/// A unit of a record in [TrackHash].
const UNIT_RECORD: u8 = 2;

impl TrackHash {
    /// The hash of a track with nothing written yet: 32 zero bytes.
    pub const START: TrackHash = TrackHash([0; 32]);

    /// The hash after the chunk `chunk`, whose payload hashes to `payload` ([payload_hash]).
    pub fn chunk(&self, chunk: &crate::Chunk<'_>, payload: &[u8; 32]) -> TrackHash {
        let head = chunk.head();
        let (times, columns) = chunk.descriptor();
        let mut h = blake3::Hasher::new();
        h.update(&self.0);
        h.update(&[UNIT_CHUNK, head.kind.to_u8()]);
        h.update(&head.count.to_le_bytes());
        h.update(times);
        h.update(columns);
        h.update(payload);
        TrackHash(*h.finalize().as_bytes())
    }

    /// The hash after a record at `ticks` (0 without a time) holding `item`.
    pub fn record(&self, ticks: i64, item: &[u8]) -> TrackHash {
        let mut h = blake3::Hasher::new();
        h.update(&self.0);
        h.update(&[UNIT_RECORD]);
        h.update(&ticks.to_le_bytes());
        h.update(&crate::chunk::len_u64(item.len()).to_le_bytes());
        h.update(item);
        TrackHash(*h.finalize().as_bytes())
    }
}

/// BLAKE3 of a chunk's payload ([crate::Chunk::payload]): what stands for it in the content hash and the track hash.
pub fn payload_hash(payload: &[u8]) -> [u8; 32] {
    *blake3::hash(payload).as_bytes()
}

/// Feed the good block `block` to a piece's content hash: a chunk's payload by its hash, which is returned with the
/// chunk; any other block, or one that doesn't read as a chunk, as it is.
pub(crate) fn hash_block<'b>(
    h: &mut blake3::Hasher,
    block: &'b [u8],
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
            let p = payload_hash(c.payload());
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

impl PieceId {
    /// No piece id: all zeros.
    pub const NONE: PieceId = PieceId([0; 16]);

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }

    /// The id of the piece after one with `id` and `hash`: the first 16 bytes of BLAKE3 in key derivation mode
    /// ("ww_tape piece id") over the hash, then the id. The same chain gives the same ids.
    pub fn after(id: PieceId, hash: &ContentHash) -> PieceId {
        Self::derive("ww_tape piece id", &[&hash.0, &id.0])
    }

    /// The id of the first piece of chain 0 of `owner`: [Self::first_of] `(owner, 0)`.
    pub fn first(owner: &OwnerId) -> PieceId {
        Self::first_of(owner, 0)
    }

    /// The id of the first piece of chain `chain` of `owner` (several chains of one owner, such as one per
    /// source): as [Self::after] over the owner id and the chain number, u64 little endian ("ww_tape first
    /// piece").
    pub fn first_of(owner: &OwnerId, chain: u64) -> PieceId {
        Self::derive("ww_tape first piece", &[&owner.0, &chain.to_le_bytes()])
    }

    /// The first 16 bytes of BLAKE3 in key derivation mode over `parts`; never all zeros.
    fn derive(context: &str, parts: &[&[u8]]) -> PieceId {
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
        PieceId(out)
    }
}

/// How a piece ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FinishState {
    /// The recording ended here.
    Complete,
    /// The recording goes on in the next piece, the one the tail names.
    Continued,
    /// The writer stopped on purpose without completing it (the producer failed); everything written is good.
    Aborted,
}

impl FinishState {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(FinishState::Complete),
            2 => Some(FinishState::Continued),
            3 => Some(FinishState::Aborted),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            FinishState::Complete => 1,
            FinishState::Continued => 2,
            FinishState::Aborted => 3,
        }
    }
}

/// Where a piece sits in its chain, as its head, finish and tail say.
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
    pub track_id: u64,
    pub track_number: u32,
    /// The first item number of the track in this piece: one past the last in the piece before.
    pub next_item: u64,
    /// No item of the track in this piece starts before this tick: the last start in the piece before.
    pub from_tick: i64,
    /// [TrackDef::owner](crate::TrackDef::owner): with the id, which track this is.
    pub owner: OwnerId,
    /// The track's hash before this piece.
    pub hash: TrackHash,
}

/// The items and ticks of one track's chunk blocks in a piece, its counted records and its hash, from its finish.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrackRange {
    pub track_id: u64,
    pub track_number: u32,
    /// Chunk blocks of the track in the piece; 0 for a track of records alone, whose range fields are 0.
    pub chunks: u32,
    pub first_item: u64,
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
}

impl TrackRange {
    /// Item number past the last.
    pub fn end_item(&self) -> u64 {
        self.first_item.saturating_add(self.items)
    }
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
    let rows = b
        .get_mut(HEAD_TRACKS_AT..)?
        .as_chunks_mut::<HEAD_TRACK_LEN>()
        .0
        .iter_mut();
    for (row, t) in rows.zip(tracks) {
        put_u64(row, 0, t.track_id)?;
        put_u32(row, 8, t.track_number)?;
        put_u64(row, 16, t.next_item)?;
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
        let head = read_piece_head(block.bytes()).ok_or(IndexError::BadHead)?;
        Ok(PieceHeadBlock { head, rows, row })
    }

    pub fn head(&self) -> PieceHead {
        self.head
    }

    pub fn tracks(&self) -> impl Iterator<Item = TrackStart> + '_ {
        self.rows.chunks_exact(self.row).map_while(|row| {
            Some(TrackStart {
                track_id: le_u64(row, 0)?,
                track_number: le_u32(row, 8)?,
                next_item: le_u64(row, 16)?,
                from_tick: le_i64(row, 24)?,
                owner: OwnerId(array(row, 32)?),
                hash: TrackHash(array(row, 64)?),
            })
        })
    }
}

fn read_piece_head(b: &[u8]) -> Option<PieceHead> {
    let prev = if le_u32(b, 88)? & FLAG_HAS_PREV != 0 {
        Some((ContentHash(array(b, 96)?), PieceId(array(b, 128)?)))
    } else {
        None
    };
    Some(PieceHead {
        ident: get_ident(b, 32, 48, 80)?,
        prev,
        prev_session: SessionTag::from_u32(le_u32(b, 92)?),
    })
}

/// The fixed part of a finish block.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Finish {
    pub state: FinishState,
    pub hash: ContentHash,
    /// The newest checkpoint index block.
    pub checkpoint: Option<u64>,
    /// The last final index block (each points at the one before).
    pub final_index: Option<u64>,
    /// The piece's ident; None for a file written without a piece head.
    pub ident: Option<PieceIdent>,
    /// The next piece's id, chosen at the cut: some when [FinishState::Continued].
    pub next: Option<PieceId>,
}

/// Bytes of a finish block with `tracks`, without padding.
pub const fn finish_len(tracks: usize) -> usize {
    block_len_of(FINISH_FIXED_LEN, tracks, FINISH_TRACK_LEN)
}

/// Zero bytes a finish block of `tracks` placed at `finish_offset` in its piece is padded with, so the piece ends,
/// after its tail, on a multiple of [PIECE_ALIGN]; below [PIECE_ALIGN].
pub const fn finish_padding(finish_offset: u64, tracks: usize) -> usize {
    // only the low bits count: the end modulo 2^64 has them
    let end = finish_offset
        .wrapping_add(len_u64(finish_len(tracks)))
        .wrapping_add(len_u64(TAIL_LEN));
    to_boundary(end, PIECE_ALIGN.wrapping_sub(1))
}

fn opt(v: Option<u64>) -> u64 {
    v.unwrap_or(NONE)
}

fn get_opt(b: &[u8], at: usize) -> Option<Option<u64>> {
    le_u64(b, at).map(|v| Some(v).filter(|v| *v != NONE))
}

/// Write a finish block with `padding` zero bytes after its tracks into `buf` (at least [finish_len] plus `padding`
/// long); returns its length. A padding of [PIECE_ALIGN] or more is refused.
pub fn write_finish(
    buf: &mut [u8],
    id: BlockId,
    finish: &Finish,
    tracks: &[TrackRange],
    padding: usize,
) -> Result<usize, IndexError> {
    let pad = u32::try_from(padding)
        .ok()
        .filter(|p| *p < PIECE_ALIGN)
        .ok_or(IndexError::Invalid("finish padding of 4 KiB or more"))?;
    let len = finish_len(tracks.len()).saturating_add(padding);
    let (b, count) = block_buf(buf, len, tracks.len())?;
    put_header(b, BlockKind::Finish, len, count, id)
        .ok_or(IndexError::Invalid("longer than a block"))?;
    fill_finish(b, finish, tracks).ok_or(IndexError::Invalid("finish layout"))?;
    put_u32(b, 172, pad).ok_or(IndexError::Invalid("finish layout"))?;
    seal(b);
    Ok(len)
}

/// The body of the finish block `b`, sized for `tracks`.
fn fill_finish(b: &mut [u8], finish: &Finish, tracks: &[TrackRange]) -> Option<()> {
    b.get_mut(HEADER_LEN..)?.fill(0);
    put(b, 32, [finish.state.to_u8()])?;
    put(b, 40, finish.hash.0)?;
    put_u64(b, 72, opt(finish.checkpoint))?;
    put_u64(b, 80, opt(finish.final_index))?;
    put_u64(b, 88, opt(None))?;
    if let Some(ident) = &finish.ident {
        put_ident(b, 96, 128, 160, ident)?;
        put_u32(b, 168, u32::from(FLAG_PIECE))?;
    }
    put(b, 112, finish.next.unwrap_or(PieceId::NONE).0)?;
    put_u16(b, FINISH_ROW_LEN_AT, u16::try_from(FINISH_TRACK_LEN).ok()?)?;
    let rows = b
        .get_mut(FINISH_TRACKS_AT..)?
        .as_chunks_mut::<FINISH_TRACK_LEN>()
        .0
        .iter_mut();
    for (row, t) in rows.zip(tracks) {
        put_u64(row, 0, t.track_id)?;
        put_u32(row, 8, t.track_number)?;
        put_u32(row, 12, t.chunks)?;
        put_u64(row, 16, t.first_item)?;
        put_u64(row, 24, t.items)?;
        put_i64(row, 32, t.first_tick)?;
        put_i64(row, 40, t.last_tick)?;
        put_i64(row, 48, t.latest_end)?;
        put(row, 56, t.owner.0)?;
        put(row, 88, t.hash.0)?;
        put_u64(row, 120, t.records)?;
    }
    Some(())
}

/// A finish block read in place.
#[derive(Copy, Clone, Debug)]
pub struct FinishBlock<'b> {
    finish: Finish,
    /// The tracks, `row` bytes each
    rows: &'b [u8],
    /// At least [FINISH_TRACK_LEN]
    row: usize,
}

impl<'b> FinishBlock<'b> {
    /// The finish of a good block.
    pub fn read(block: &Block<'b>) -> Result<Self, IndexError> {
        of_kind(block, BlockKind::Finish)?;
        let padding = le_u32(block.bytes(), 172)
            .filter(|p| *p < PIECE_ALIGN)
            .map(u32_len)
            .ok_or(IndexError::BadHead)?;
        let (rows, row) = body(
            block,
            BlockKind::Finish,
            FINISH_FIXED_LEN,
            (FINISH_ROW_LEN_AT, FINISH_TRACK_LEN),
            padding,
        )?;
        let finish = read_finish(block.bytes()).ok_or(IndexError::BadHead)?;
        Ok(FinishBlock { finish, rows, row })
    }

    pub fn finish(&self) -> Finish {
        self.finish
    }

    pub fn tracks(&self) -> impl Iterator<Item = TrackRange> + '_ {
        self.rows.chunks_exact(self.row).map_while(|row| {
            Some(TrackRange {
                track_id: le_u64(row, 0)?,
                track_number: le_u32(row, 8)?,
                chunks: le_u32(row, 12)?,
                first_item: le_u64(row, 16)?,
                items: le_u64(row, 24)?,
                first_tick: le_i64(row, 32)?,
                last_tick: le_i64(row, 40)?,
                latest_end: le_i64(row, 48)?,
                owner: OwnerId(array(row, 56)?),
                hash: TrackHash(array(row, 88)?),
                records: le_u64(row, 120)?,
            })
        })
    }
}

/// The fixed part of the finish block `b`; None when its state is none this reader knows.
fn read_finish(b: &[u8]) -> Option<Finish> {
    let piece = le_u32(b, 168)? & u32::from(FLAG_PIECE) != 0;
    let next = PieceId(array(b, 112)?);
    let ident = if piece {
        Some(get_ident(b, 96, 128, 160)?)
    } else {
        None
    };
    Some(Finish {
        state: FinishState::from_u8(le_u8(b, 32)?)?,
        hash: ContentHash(array(b, 40)?),
        checkpoint: get_opt(b, 72)?,
        final_index: get_opt(b, 80)?,
        ident,
        next: (!next.is_none()).then_some(next),
    })
}

/// What a tail block says.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tail {
    /// Bytes from the piece's start to the end of the tail.
    pub piece_len: u64,
    pub finish_offset: u64,
    pub finish_len: u32,
    pub state: FinishState,
    pub hash: ContentHash,
    pub ident: Option<PieceIdent>,
    pub next: Option<PieceId>,
}

impl Tail {
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
        put_u64(b, 40, self.finish_offset)?;
        put_u32(b, 48, self.finish_len)?;
        put(b, 52, [self.state.to_u8()])?;
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

    /// The tail in the good tail block `b`; None when its state is unknown or its offsets don't fit the piece.
    fn read_body(b: &[u8]) -> Option<Self> {
        let state = FinishState::from_u8(le_u8(b, 52)?)?;
        let piece_len = le_u64(b, 32)?;
        let finish_offset = le_u64(b, 40)?;
        let finish_len = le_u32(b, 48)?;
        let fits = finish_offset
            .checked_add(u64::from(finish_len))?
            .checked_add(u64::try_from(TAIL_LEN).ok()?)?
            <= piece_len;
        if !fits {
            return None;
        }
        let next = PieceId(array(b, 104)?);
        let ident = if le_u8(b, 53)? & FLAG_PIECE != 0 {
            Some(get_ident(b, 88, 120, 152)?)
        } else {
            None
        };
        Some(Tail {
            piece_len,
            finish_offset,
            finish_len,
            state,
            hash: ContentHash(array(b, 56)?),
            ident,
            next: (!next.is_none()).then_some(next),
        })
    }
}

#[cfg(feature = "alloc")]
mod chain;
#[cfg(feature = "alloc")]
pub use chain::{ChainEnd, ChainLink, ChainReport, PieceSummary, check_chain};

#[cfg(feature = "std")]
mod writer;
#[cfg(feature = "std")]
pub use writer::{
    CUT_SLACK, ChainOptions, ChunkPlaced, CutPolicy, PieceChain, PieceFiles, piece_name,
};

#[cfg(test)]
mod tests;
