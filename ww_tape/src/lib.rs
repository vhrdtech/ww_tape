//! Tape blocks: the unit a producer of records hands over, a live consumer reads and a log file is made of.
//!
//! A tape is the recording unit whatever it is recorded on: blocks handed over live, appended to a file, replayed.
//!
//! A **record** is an item (any ShrinkWrap type, or plain bytes) on a **track**, at a time: signed i64 ticks of the
//! track's **clock**. Clocks are described by a [ClockDef] (number, stable key, exact rate and resolution, origin),
//! tracks by a [TrackDef]: a small number used in records, a stable key, the clock, dimensions that are part of its
//! identity, the track's resource path in the producer's API (`ww_trait` ids, array indices included) and the item
//! type, so a reader knows from the API bundle what the items are. How clocks relate (the wall clock to a device's) is
//! said by [ClockMap]s. Nothing here knows any producer: a link tap, a device log or a simulator all write the same
//! blocks.
//!
//! A **block** is a header, a body and a CRC-32C (`CRC_32_ISCSI`):
//!
//! ```text
//! off size field
//!   0   4  magic "WWTP"
//!   4  u8  version, 1
//!   5  u8  kind: 1 records, 3 and up other kinds (BlockKind); 2 is reserved (RESERVED_KIND)
//!   6 u16  flags: bit 0 body coded (FLAG_BODY_CODED, records blocks only), bit 1 state records a writer put or
//!          copied (FLAG_REPEAT), bit 2 puts that belong with the next block (FLAG_WITH_NEXT), bit 3 on a
//!          chunk its payload unchecked (FLAG_PAYLOAD_UNCHECKED, reserved: 0, a reader refuses it), bit 4 on an
//!          events chunk a break in its track's monotone relations (FLAG_RELATION_BREAK); others 0 (a reader
//!          ignores bits it doesn't know)
//!   8 u32  len: the whole block, header and CRC included
//!  12 u32  records in the block
//!  16 u64  seq: the block number in its session, from 0, +1 per block; a gap is lost blocks. Index, close, tail and
//!          piece head blocks, and blocks marked FLAG_REPEAT, carry the seq of the block before them and are
//!          not counted (index::is_structural)
//!  24 u32  session tag (SessionTag): the same in every block of one writer's unbroken append, never 0 nor FFFFFFFFh
//!  28 u32  head CRC in chunk and index blocks (their own docs), else 0
//!  32 ...  body
//! len-4 u32 crc over bytes 0..len-4
//! ```
//!
//! The body of a records block is a clock table, then records back to back:
//!
//! ```text
//! uvar n, n × uvar clock number (ascending, 1 and up): the clocks of the block's records; slot i is the i-th
//! per record:
//!   uvar head: track << bits | slot, bits the fewest that hold n - 1 (0 for one clock)
//!   ivar time delta in ticks, against the previous record of the same slot in the block (the first against 0);
//!        absent for track 0
//!   uvar item length, the item's bytes
//! ```
//!
//! The item of a record on an events track whose [EventsDef] has spans or flags is an [EventRecord]: the duration
//! (uvar, spans only), the flags (uvar, only with flags), then the payload; the record's time is the start. A track
//! of plain instants carries the payload alone. A state record of an events track carries the same item.
//!
//! With [FLAG_BODY_CODED] that body is coded: a codec byte, its decoded length and the coded bytes ([body]).
//!
//! So time deltas are coded per clock, a block reads alone (its first time per clock is absolute) and one clock costs
//! no bit in the head. No wall clock is in a block: wall time is a clock of its own, mapped to the others.
//!
//! A record's clock is always its track's ([TrackDef::clock]). The slot repeats it on purpose, so a block's times read
//! without the definitions; a writer keeps the two in agreement, and a reader may take a record whose slot names
//! another clock than its track's for damage.
//!
//! The header and the CRC at the end are the same in every version, so any reader can check a block and skip it by its
//! length. A later version bumps the version byte and changes only the body; new block kinds and flag bits are added
//! the same way. A block of another version that goes on with the data ends the piece the reader is in and starts a
//! piece of that version, which the reader lists with its version and refuses to read ([reader::ReadError::Version]).
//! A block of an unknown kind reads as it is ([Block::body_known] false, no records), not as damage. [Describe] is an
//! evolvable enum, so a describe record of a variant a reader doesn't know reads as [Describe::Unknown] and is
//! skipped.
//!
//! Numbers are little endian; `uvar` is LEB128 (7 bits per byte, low bits first, at most 10 bytes), `ivar` zigzag
//! then `uvar`, as in the logring format. Blocks are as long as their records, never padded, except a piece's close
//! block, which ends the piece on 4 KiB ([piece]): a file is blocks appended back to back, a live feed sends one block
//! per message.
//!
//! **Track 0** holds the definitions: one [Describe] per record, its item's whole bytes: the [Producer], the
//! [Layout], the API bundles, the [ClockDef]s, [ClockMap]s and [TrackDef]s in use and typed items ([TypedItem]: a run
//! record, a media reference). Two facts about it are built in: its item type is [Describe], and its records have no
//! time (clocks are defined on it). Definitions sit in records blocks of their own (no other track in them).
//!
//! **State.** Track 0 and every track whose policy is [TrackRepeat::State] hold state, not events: a reader at any
//! place takes the nearest record of each slot behind it ([state]). A writer keeps that record close behind every use
//! ([StatePolicy::near]): before a block of data of a track it puts the track's definition and the clock, maps and
//! bundles it names, and the application puts a state track's item where it is used ([TapeWriter::put]); either is
//! written again only when the same item doesn't lie close behind in the same piece. Such writes are records in
//! blocks marked [FLAG_REPEAT], with the seq of the block before them; an item first written in a piece gets spaced
//! copies too ([StatePolicy::copies]). The index lists every state record by its slot and hash, so a reader finds
//! the item in force at a place without a scan, and a piece read alone, a file whose head is cut off and a consumer
//! joining a live feed all find what they need close behind the data.
//!
//! **Gaps.** A [Layout] says how far a writer may leave a blank or bad stretch and go on after it ([Layout::gap_max])
//! and where it resumes ([Layout::resume_align]). A reader's cost per gap is bounded by the format: `gap_max` is at
//! most [GAP_MAX_MAX] (64 MiB) with at most [GAP_PROBES_MAX] (8192) resume places in it, else the layout record
//! doesn't decode; a reader reads the first non-blank byte's and each resume place's header one at a time, and checks
//! at most [GAP_CHECKS_MAX] (4) whole blocks whose header says they go on with the data. On a raw medium ([Medium])
//! blank space longer than `gap_max` is the end of data; a piece there is one session (`TapeWriter::adopt_block`).
//!
//! **Limits.** Every count, length, offset or layout value a reader takes from the bytes is bounded before it sizes
//! an allocation, a slice or a loop ([FromFile]): by a length already in hand (the block's, a table's) or by one of
//! these caps. A value past its cap is damage, refused with an error; no file a writer of this crate writes is.
//!
//! | Cap | Value | Bounds |
//! |---|---|---|
//! | [MAX_BLOCK_LEN] | 64 MiB | a block's length |
//! | [DECODED_BODY_MAX](body::DECODED_BODY_MAX) | 64 MiB - 36 | a coded body's decoded length |
//! | [LZ4_DECODED_MAX](lz4::LZ4_DECODED_MAX) | 64 MiB | an lz4 column's decoded length |
//! | [MAX_BLOCK_CLOCKS] | 16 | the clocks of one block's clock table |
//! | [MAX_RECORD_HEAD] | 25 | a record head's bytes |
//! | [MAX_CLOCK_TABLE] | 81 | a clock table's bytes |
//! | [MAX_COLUMNS](chunk::MAX_COLUMNS) | 255 | a chunk's columns |
//! | [SEGMENT_SHIFT_MAX](chunk::SEGMENT_SHIFT_MAX) | 24 | a chunk's segment shift (16 MiB segments) |
//! | [MAX_SEGMENTS](chunk::MAX_SEGMENTS) | 16384 | a chunk's segments |
//! | [SERIES_ITEMS_MAX](chunk::SERIES_ITEMS_MAX) | 65535 | the items of a chunk with a series payloads column |
//! | [WIDTH_MAX](bitpack::WIDTH_MAX) | 64 | a bit width ([bitpack::Width]) |
//! | [KERNEL_WIDTH_MAX](bitpack::KERNEL_WIDTH_MAX) | 32 | a bit width the 32-bit kernels unpack |
//! | [SLOPE_MAX](times::SLOPE_MAX) | 2^55 | a start group's slope (Q16 ticks per item) |
//! | [ITEMS_MAX](times::ITEMS_MAX) | 16 Mi | the items of an events chunk |
//! | [CLOSE_SUMMARY_MAX](piece::CLOSE_SUMMARY_MAX) | 1 MiB | the body of the summary block a close names |
//! | [REMOVED_PIECES_MAX] | 65536 | the removed pieces one close names |
//! | [INDEX_LEN_MAX](index::INDEX_LEN_MAX) | 1 MiB | an index block a writer makes, and a reader looks for from the end |
//! | [CHECKPOINT_ENTRIES_MAX](index::CHECKPOINT_ENTRIES_MAX) | 8737 | entries a writer puts in a checkpoint index block |
//! | [NODE_SHIFT_MAX](summary::NODE_SHIFT_MAX) | 62 | a summary node's items, as a power of 2 |
//! | [LEVEL_MAX](summary::LEVEL_MAX) | 15 | a summary tree's level |
//! | [SCALE_LEN_MAX](series::SCALE_LEN_MAX) | 64 | the scale bytes of a series chunk or summary |
//! | [SeriesSummarySpec::BASE_SHIFT_MAX](summary::SeriesSummarySpec::BASE_SHIFT_MAX) | 24 | a series summary's base node, as a power of 2 |
//! | [SERIES_CHANNELS_MAX](series::SERIES_CHANNELS_MAX) | 65535 | a series track's channels |
//! | [GAP_MAX_MAX] | 64 MiB | a layout's gap |
//! | [GAP_PROBES_MAX] | 8192 | the resume places a reader probes in one gap |
//! | [GAP_CHECKS_MAX] | 4 | the blocks a reader checks whole past one gap |
//! | [RESYNC_AHEAD_MAX] | 128 MiB | the bytes a salvage walk ([Recovery::Resync]) reads ahead |
//! | [RESYNC_SEQS_MAX] | 65536 | the data blocks a salvage walk remembers by session and seq, for conflicts |
//! | [STATE_READ_MAX](reader::STATE_READ_MAX) | 1 GiB | the bytes of blocks one state lookup reads |
//! | [VERIFY_LISTS_MAX](reader::VERIFY_LISTS_MAX) | 466432 | the track lists of a final index one verify takes, one entry held each |
//! | [DEFERRED_BYTES_MAX](writer::DEFERRED_BYTES_MAX) | 1 MiB | the deferred blocks a writer keeps |
//! | [TRACKS_PER_PIECE_MAX] | 466430 | the tracks with data in one piece a writer takes: its close fits a block |
//! | [HEAD_TRACKS_MAX] | 699006 | the tracks a piece head carries over the chain: the head fits a block |
//! | [WALK_MISS_BASE_MAX](piece::WALK_MISS_BASE_MAX) | 1 MiB | the bytes a content-hash walk's missed blocks claim at its start |
//! | [WALK_MISS_PER_BYTE_MAX](piece::WALK_MISS_PER_BYTE_MAX) | 4 | the bytes they claim per byte walked, past that |
//! | [RESUME_LOOK_MAX](writer::RESUME_LOOK_MAX) | 128 MiB | the bytes past a reader's stop a resume looks through for a good block |
//! | [RESUME_PROBES_MAX](writer::RESUME_PROBES_MAX) | 4 | the places there it reads whole |
//!
//! The same input gives the same bytes: blocks hold no wall time, definitions go in the order they are made, what a
//! writer puts and copies follows from the blocks before ([writer]) and blocks are cut by size, not by time, unless
//! the producer seals them on a timer.
//! One exception: an [lz4] payload column is identical only for one `lz4_flex` version on targets of the
//! same pointer width and endianness (decoding is unaffected).

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unreachable,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::cast_lossless,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "tests fail by panicking and compute expected values freely"
    )
)]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "std")]
pub use ww_version::FullVersionOwned;
pub use ww_version::{FullVersion, Version};

mod describe;
pub use describe::{
    ALIGNMENT, ClockDef, ClockMap, ClockOrigin, ClockRate, ClockResolution, ClockSpec, Describe,
    Dimension, ENCODING_NAME_MAX, Ends, EventsDef, Foreign, GAP_CHECKS_MAX, GAP_MAX_MAX,
    GAP_PROBES_MAX, Items, KeySegment, Layout, MEDIA_TYPE_MAX, MapSegment, NEAR_DEFAULT, Producer,
    RELATION_NAME_MAX, RELATIONS_MAX, RelationDef, RelationIndex, RelationTarget, RelationValues,
    SCHEMA_HASH_LEN, SD_GAP_MAX, SD_RESUME_ALIGN, Schema, StatePolicy, TrackDef, TrackKind,
    TrackRepeat, TrackSpec, TypeRef, TypedItem, UNIX_CLOCK_KEY, api, clock_key, media_type_valid,
    segments_increasing, track_id,
};
#[cfg(feature = "std")]
pub use describe::{
    ClockDefOwned, ClockMapOwned, DescribeOwned, DimensionOwned, ForeignOwned, ItemsOwned,
    KeySegmentOwned, ProducerOwned, RelationDefOwned, SchemaOwned, TrackDefOwned, TrackKindOwned,
    TypeRefOwned, TypedItemOwned,
};
pub use exact_units::{Magnitude, MagnitudeOf, Ratio64, Rounding};
mod numbers;
pub use numbers::{
    ClockKey, ClockNum, ItemNum, RecordTrack, RelationSet, TrackId, TrackKey, TrackNum,
};
pub mod state;
pub use state::{DefinitionKind, ItemHash, Slot};

pub mod summary;
pub use summary::{EventFields, SummaryError, SummaryHead};

pub use ww_tape_codecs::bitpack;
pub mod chunk;
pub mod times;
pub use chunk::{
    CHUNK_HEAD_PREFIX, Chunk, ChunkError, ChunkHead, ChunkHeadError, ChunkWriter, Codec,
    ColumnKind, ColumnSpec, EndMode,
};

pub use from_file::{BoundError, FromFile};
use ww_tape_codecs::bytes;
use ww_tape_codecs::crc32c;
pub use ww_tape_codecs::from_file;
pub use ww_tape_codecs::lz4;
#[cfg(feature = "alloc")]
pub mod payloads;
pub mod samples;
pub use samples::{
    ELEMENT_PARTS_MAX, Element, ElementDesc, Sample, SampleError, SampleShape, Samples,
};

pub mod series;
pub use series::{MediaStream, SeriesDef, SeriesError, SeriesGrid, SeriesValue};
#[cfg(feature = "std")]
pub use series::{MediaStreamOwned, SeriesDefOwned};
#[cfg(feature = "alloc")]
pub use series::{SeriesStretch, SeriesTimeline};

pub mod index;
pub use index::{
    IndexBlock, IndexEntry, IndexError, IndexHead, IndexKind, IndexPolicy, StateEntry,
};
pub mod piece;
#[cfg(feature = "alloc")]
pub use piece::{ChainOptions, ChainPiece, ChunkPlaced, CutPolicy, PieceChain, PieceTapes};
pub use piece::{
    Close, CloseReason, ContentHash, HEAD_TRACKS_MAX, Integrity, OwnerId, PIECE_ALIGN, PieceHead,
    PieceId, PieceIdent, REMOVED_PIECES_MAX, RemovedPiece, TAIL_LEN, TRACKS_PER_PIECE_MAX, Tail,
    TrackHash, TrackRange, TrackStart, UnitHash,
};
#[cfg(feature = "alloc")]
pub mod reader;
#[cfg(feature = "alloc")]
pub use reader::{
    ByteSource, OpenOptions, PieceInfo, RangeHash, ReadError, TapeReader, Verification,
};

mod event;
pub use event::{EventRecord, EventRecordError};
pub mod body;
pub use body::{BodyCodec, CodeError, code_body, coded_len_max};
mod session;
pub use session::{BlockId, Medium, SessionTag};
use ww_tape_codecs::varint;
#[cfg(feature = "alloc")]
mod walk;
pub use varint::{ivar_len, uvar_len};

#[cfg(feature = "std")]
mod stream;
#[cfg(feature = "std")]
pub use stream::{
    BlockBuf, Cut, FileBlocks, FileError, RESYNC_AHEAD_MAX, RESYNC_SEQS_MAX, Recovery,
};
#[cfg(feature = "alloc")]
pub mod open;
#[cfg(feature = "alloc")]
pub use open::{Definitions, DescribeError, Numbers, RecordRef, STATE_ITEM_MAX, TrackFacts};
#[cfg(feature = "alloc")]
pub mod sink;
#[cfg(feature = "std")]
pub use sink::{MemError, MemSink, MemTape, MemTapes};
#[cfg(feature = "alloc")]
pub use sink::{TapeSink, Tapes};
#[cfg(feature = "alloc")]
mod clock;
#[cfg(feature = "std")]
pub use clock::SystemClock;
#[cfg(feature = "alloc")]
pub use clock::{Clock, ClockRef};
#[cfg(feature = "alloc")]
pub mod writer;
#[cfg(feature = "alloc")]
pub use writer::{
    CARRIED_BLOCK_LEN, Closed, CutError, DEFER_WINDOW, HEAD_ENTRIES, InputError, NearPolicy,
    Placed, PutWork, RemovedRefusal, ResumeError, Resumed, SyncPolicy, TapeWriter, WriteError,
    WriterOptions,
};

/// `WWTP`
pub const MAGIC: [u8; 4] = *b"WWTP";
/// The format version: 1, the first.
pub const VERSION: u8 = 1;
/// Bytes of the block header.
pub const HEADER_LEN: usize = 32;
/// Longest block a reader takes (64 MiB): a longer length is damage. Blocks are small (a producer hands over 64 KiB
/// ones); the largest are blocks of definitions with big API bundles.
pub const MAX_BLOCK_LEN: usize = 64 << 20;
const _: () = assert!(
    MAX_BLOCK_LEN >> 31 == 0,
    "a block length is a u32 in the header"
);
const _: () = assert!(
    lz4::LZ4_DECODED_MAX == MAX_BLOCK_LEN,
    "an lz4 column decodes to at most a whole block"
);
/// Header flag: the block holds state records a writer put or copied ([state]), not new items of the producer's: it
/// carries the seq of the block before it and takes no part in the seq order.
///
/// A reader holding an item of the same hash in the same slot takes such a record as nothing new.
pub const FLAG_REPEAT: u16 = 1 << 1;
/// Header flag, on a block marked [FLAG_REPEAT] only: the block belongs with the block after it.
///
/// A run of such blocks ends in the block they belong to (the block of data their puts serve, or the last block of a
/// put's records), all of one writer call.
///
/// A writer resumed after a crash drops such blocks at the end of what it keeps, and the call given again writes them
/// as it did. A reader takes the block as any other.
pub const FLAG_WITH_NEXT: u16 = 1 << 2;
/// Header flag on chunk blocks: the payload's CRCs were not computed, set on each chunk of a piece of
/// [Integrity::None].
///
/// Written 0; a reader of this build refuses such a chunk as [BlockError::PayloadUnchecked], a level it doesn't handle,
/// not damage.
pub const FLAG_PAYLOAD_UNCHECKED: u16 = 1 << 3;
/// Header flag on events chunk blocks: the order of its track's monotone relations starts again at this chunk.
///
/// Its first values were checked against a value the writer's caller gave
/// ([TapeWriter::accept_relation_break](crate::TapeWriter::accept_relation_break)), not against the chunks before it,
/// so a reader takes no order across its start. Its index entry carries the mark too
/// ([IndexEntry::relation_break](crate::index::IndexEntry::relation_break)).
pub const FLAG_RELATION_BREAK: u16 = 1 << 4;
/// Block kind 2, which earlier drafts gave describe blocks: reserved, never written, read as a kind this reader
/// doesn't know.
pub const RESERVED_KIND: u8 = 2;
/// Header flag: the body of a records block is coded ([body]); on another kind the body is one this
/// reader doesn't know. Off unless a writer codes the block with [code_body].
pub const FLAG_BODY_CODED: u16 = 1 << 0;
/// Bytes of the CRC at the end of a block.
pub const CRC_LEN: usize = 4;
/// Most clocks in one block's clock table; a producer writing on more starts another block.
pub const MAX_BLOCK_CLOCKS: usize = 16;
/// Longest record head: head, time delta and length varints.
pub const MAX_RECORD_HEAD: usize = 10 + 10 + 5;
/// Longest clock table.
pub const MAX_CLOCK_TABLE: usize = 1 + 5 * MAX_BLOCK_CLOCKS;

const CRC: crc32c::Crc32c = crc32c::Crc32c;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// Records of any tracks, interleaved.
    Records,
    /// One chunk of one event track ([chunk]).
    Events,
    /// One chunk of one fixed-rate series ([chunk]).
    Series,
    /// One batch of nodes of one level of a track's summary tree ([summary]).
    Summary,
    /// Reserved for parity blocks (one per group of blocks, for archives on poor media); none are written yet, and a
    /// reader skips them like a kind it doesn't know.
    Parity,
    /// A checkpoint or final index of the piece's blocks ([index]).
    Index,
    /// How a piece ended, with its content hash ([piece]).
    Close,
    /// The fixed-size last block of a piece ([piece]).
    Tail,
    /// The first block of a piece of a chain ([piece]).
    PieceHead,
    /// The first block written on after a piece's close record and tail, naming them ([piece]).
    AppendHead,
    /// A block of the caller's own a close names, placed right before its final index: its body is the caller's,
    /// opaque to the format ([piece::write_close_summary]).
    CloseSummary,
    /// A kind this reader doesn't know: its body isn't read.
    Other(u8),
}

impl BlockKind {
    /// Reserved, kind 11: per events chunk, a relation's postings, each distinct value with the items holding it
    /// ([RelationIndex::Postings]). Not built: read as [BlockKind::Other], skipped by its length.
    pub const POSTINGS: Self = Self::Other(11);
    /// Reserved, kind 12: per events chunk, a relation's dictionary, the distinct values and each item's index into
    /// them ([RelationIndex::Dictionary]). Not built: read as [BlockKind::Other], skipped by its length.
    pub const DICTIONARY: Self = Self::Other(12);

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Records,
            3 => Self::Events,
            4 => Self::Series,
            5 => Self::Parity,
            6 => Self::Summary,
            7 => Self::Index,
            8 => Self::Close,
            9 => Self::Tail,
            10 => Self::PieceHead,
            13 => Self::AppendHead,
            14 => Self::CloseSummary,
            other => Self::Other(other),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Records => 1,
            Self::Events => 3,
            Self::Series => 4,
            Self::Parity => 5,
            Self::Summary => 6,
            Self::Index => 7,
            Self::Close => 8,
            Self::Tail => 9,
            Self::PieceHead => 10,
            Self::AppendHead => 13,
            Self::CloseSummary => 14,
            Self::Other(v) => v,
        }
    }
}

/// Whether `b` is a blank byte: 0x00 (zeros, preallocated space, a trimmed SD card or eMMC) or 0xFF (erased NAND, an SD
/// card erased to ones).
///
/// A block starts with `W` (0x57), never with a blank byte, and a stretch of blank bytes (any mix of the two) where a
/// block would start is no block.
pub const fn is_blank(b: u8) -> bool {
    b == 0x00 || b == 0xFF
}

/// FNV-1a, 64 bit.
/// A cap given as a `u64` as a `usize`: `usize::MAX` where it is past the address space, since nothing in memory
/// reaches it then.
#[cfg(feature = "alloc")]
pub(crate) fn cap_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

pub const fn fnv1a(mut h: u64, mut bytes: &[u8]) -> u64 {
    while let [b, rest @ ..] = bytes {
        #[expect(clippy::as_conversions, reason = "u8 to u64 widens; From is not const")]
        let b = *b as u64;
        h ^= b;
        h = h.wrapping_mul(0x100000001b3);
        bytes = rest;
    }
    h
}

pub const FNV_OFFSET: u64 = 0xcbf29ce484222325;

/// Names an API across all its versions: FNV-1a of the crate name. A breaking change is a new definition generation
/// of the same API, not a new identity.
pub const fn api_root_id(crate_name: &str) -> u64 {
    fnv1a(FNV_OFFSET, crate_name.as_bytes())
}

/// One step of a track's name path, the input of [track_key].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Segment<'a> {
    /// A resource's identity name: its name, or for a renamed resource the oldest name of its `#[moved_from]` chain.
    Name(&'a str),
    /// An index into a plain resource array.
    Index(u32),
    /// The key of an element of a keyed resource array: the key's shrink_wrap bytes. Two sessions that
    /// number the elements differently give the same key.
    Key(&'a [u8]),
}

impl Segment<'_> {
    /// Feed the segment into an FNV-1a state: a tag byte, the length of its bytes (u32 LE), the bytes. Tag and length
    /// keep `a.b` and `ab`, or a name and a key of the same bytes, apart.
    const fn hash(&self, h: u64) -> u64 {
        let index;
        let (tag, bytes): (u8, &[u8]) = match self {
            Segment::Name(name) => (1, name.as_bytes()),
            Segment::Index(i) => {
                index = i.to_le_bytes();
                (2, &index)
            }
            Segment::Key(key) => (3, key),
        };
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "a hash input: a name or key over 4 GiB only hashes a shorter length; TryFrom is not const"
        )]
        let len = bytes.len() as u32;
        fnv1a(fnv1a(fnv1a(h, &[tag]), &len.to_le_bytes()), bytes)
    }
}

/// Stable key of a track: FNV-1a of `api_root` (u64 LE), then of every segment of the resource's name path (see
/// [Segment]), so the key survives renumbered ids and indices assigned in another order.
pub const fn track_key(api_root: u64, mut segments: &[Segment<'_>]) -> TrackKey {
    let mut h = fnv1a(FNV_OFFSET, &api_root.to_le_bytes());
    while let [segment, rest @ ..] = segments {
        h = segment.hash(h);
        segments = rest;
    }
    TrackKey::new(h)
}

/// A record's time: ticks of a clock.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClockTime {
    pub clock: ClockNum,
    pub ticks: i64,
}

impl ClockTime {
    pub const fn new(clock: ClockNum, ticks: i64) -> Self {
        Self { clock, ticks }
    }
}

/// Bits of the slot in a record head for `n` clocks.
const fn slot_bits(n: usize) -> u32 {
    match n.checked_sub(1) {
        None | Some(0) => 0,
        #[expect(
            clippy::arithmetic_side_effects,
            reason = "m is nonzero: at most usize::BITS - 1 leading zeros"
        )]
        Some(m) => usize::BITS - m.leading_zeros(),
    }
}

/// The clocks of a block and the last time on each.
#[derive(Copy, Clone, Debug)]
struct Slots {
    /// The first [Self::len] are the block's; the rest are filler.
    clocks: [ClockNum; MAX_BLOCK_CLOCKS],
    last: [i64; MAX_BLOCK_CLOCKS],
    n: u8,
}

impl Slots {
    const NONE: Self = Self {
        clocks: [ClockNum::MIN; MAX_BLOCK_CLOCKS],
        last: [0; MAX_BLOCK_CLOCKS],
        n: 0,
    };

    /// Slots in use; `n` is at most [MAX_BLOCK_CLOCKS] where it is set.
    fn len(&self) -> usize {
        usize::from(self.n).min(MAX_BLOCK_CLOCKS)
    }

    fn clocks(&self) -> &[ClockNum] {
        self.clocks.get(..self.len()).unwrap_or_default()
    }

    fn bits(&self) -> u32 {
        slot_bits(self.len())
    }

    fn slot(&self, clock: ClockNum) -> Option<usize> {
        self.clocks().binary_search(&clock).ok()
    }
}

/// The block does not fit: nothing was written.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Full;

/// Why [BlockWriter::new] can't start a block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockStartError {
    /// More clocks than [MAX_BLOCK_CLOCKS].
    TooManyClocks,
    /// The clocks are not strictly ascending.
    ClocksUnordered,
    /// The buffer doesn't hold the header, the clock table and the CRC: `needed` bytes.
    BufferTooSmall { needed: usize },
}

/// Why [BlockWriter::push] or [BlockWriter::push_with] wrote nothing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushError {
    /// The record doesn't fit the block.
    Full,
    /// The record's clock is not one the block was started with.
    UnknownClock(ClockNum),
}

impl From<Full> for PushError {
    fn from(_: Full) -> Self {
        Self::Full
    }
}

/// Writes one block into a buffer: [BlockWriter::push] records, then [BlockWriter::finish].
pub struct BlockWriter<'b> {
    buf: &'b mut [u8],
    state: BlockState,
}

impl<'b> BlockWriter<'b> {
    /// Start a block in `buf` holding records on `clocks` (strictly ascending; at most [MAX_BLOCK_CLOCKS]); at most [MAX_BLOCK_LEN] of `buf` is used. A `buf` that doesn't hold
    /// the header, the clock table and the CRC is refused.
    pub fn new(
        buf: &'b mut [u8],
        kind: BlockKind,
        id: BlockId,
        clocks: &[ClockNum],
    ) -> Result<Self, BlockStartError> {
        if clocks.len() > MAX_BLOCK_CLOCKS {
            return Err(BlockStartError::TooManyClocks);
        }
        if !clocks.is_sorted_by(|a, b| a < b) {
            return Err(BlockStartError::ClocksUnordered);
        }
        let n = u8::try_from(clocks.len())
            .ok()
            .ok_or(BlockStartError::TooManyClocks)?;
        let table =
            || core::iter::once(u64::from(n)).chain(clocks.iter().map(|c| u64::from(c.get())));
        // at most MAX_CLOCK_TABLE bytes: 16 clocks of 5 bytes and the count
        let needed = table()
            .map(uvar_len)
            .try_fold(HEADER_LEN + CRC_LEN, usize::checked_add)
            .ok_or(BlockStartError::TooManyClocks)?;
        let small = BlockStartError::BufferTooSmall { needed };
        let usable = buf.len().min(MAX_BLOCK_LEN);
        if usable < needed {
            return Err(small);
        }
        let buf = buf.get_mut(..usable).ok_or(small)?;
        put_header(buf, kind, 0, 0, id).ok_or(small)?;
        let mut pos = HEADER_LEN;
        for v in table() {
            let at = buf.get_mut(pos..).ok_or(small)?;
            pos = pos
                .checked_add(varint::put_uvar(at, v).ok_or(small)?)
                .ok_or(small)?;
        }
        let mut slots = Slots::NONE;
        slots.n = n;
        for (slot, c) in slots.clocks.iter_mut().zip(clocks) {
            *slot = *c;
        }
        Ok(BlockWriter {
            buf,
            state: BlockState {
                pos,
                body: pos,
                records: 0,
                slots,
            },
        })
    }

    /// Release the buffer, keeping what is needed to go on with [Self::from_state] on the same buffer.
    pub fn into_state(self) -> BlockState {
        self.state
    }

    /// Go on writing a block started in `buf` (the same bytes, unchanged since [Self::into_state]).
    ///
    /// # Panics
    ///
    /// When `buf` is shorter than the block written so far and its CRC: not the buffer of `state`.
    pub fn from_state(buf: &'b mut [u8], state: BlockState) -> Self {
        let usable = buf.len().min(MAX_BLOCK_LEN);
        assert!(
            state
                .pos
                .checked_add(CRC_LEN)
                .is_some_and(|end| end <= usable),
            "from_state takes the buffer the block was started in"
        );
        let (buf, _) = buf.split_at_mut(usable);
        BlockWriter { buf, state }
    }

    /// Bytes of records written so far.
    pub fn used(&self) -> usize {
        self.state.pos.saturating_sub(self.state.body)
    }

    pub fn records(&self) -> u32 {
        self.state.records
    }

    /// The clocks the block was started with.
    pub fn clocks(&self) -> &[ClockNum] {
        self.state.slots.clocks()
    }

    /// Free bytes for records, the CRC left aside.
    pub fn free(&self) -> usize {
        // new and from_state hold pos + CRC_LEN <= buf.len(), put keeps it
        self.buf
            .len()
            .saturating_sub(CRC_LEN)
            .saturating_sub(self.state.pos)
    }

    /// Add a record on `track` at `at` whose item is `parts` back to back; `at`'s clock is the track's
    /// [TrackDef::clock]. A clock that isn't one of the block's is refused; definitions go through
    /// [Self::push_describe].
    pub fn push(
        &mut self,
        track: TrackNum,
        at: ClockTime,
        parts: &[&[u8]],
    ) -> Result<(), PushError> {
        self.push_with(track, at, |buf| copy_parts(buf, parts))
    }

    /// Add a describe record (track 0, no time) whose item is `parts` back to back.
    pub fn push_describe(&mut self, parts: &[&[u8]]) -> Result<(), Full> {
        self.push_describe_with(|buf| copy_parts(buf, parts))
    }

    /// [Self::push] with an item `encode` writes into the free space and returns the length of, so a ShrinkWrap
    /// value is serialized in place without a copy. `encode` returning None (did not fit) leaves the block as it was.
    pub fn push_with(
        &mut self,
        track: TrackNum,
        at: ClockTime,
        encode: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> Result<(), PushError> {
        let slots = &self.state.slots;
        let slot = slots
            .slot(at.clock)
            .ok_or(PushError::UnknownClock(at.clock))?;
        // a track number of 32 bits shifted by at most 4 slot bits fits a u64
        let head = (u64::from(track.get()) << slots.bits()) | bytes::u64_of(slot);
        let last = slots
            .last
            .get(slot)
            .ok_or(PushError::UnknownClock(at.clock))?;
        let dt = at.ticks.wrapping_sub(*last);
        self.put(head, Some(dt), encode)?;
        if let Some(last) = self.state.slots.last.get_mut(slot) {
            *last = at.ticks;
        }
        Ok(())
    }

    /// [Self::push_describe] with an item `encode` writes in place, as [Self::push_with].
    pub fn push_describe_with(
        &mut self,
        encode: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> Result<(), Full> {
        self.put(0, None, encode)
    }

    /// Write a record head, its time and an item `encode` writes; nothing is kept when any part doesn't fit.
    fn put(
        &mut self,
        head: u64,
        dt: Option<i64>,
        encode: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> Result<(), Full> {
        let records = self.state.records.checked_add(1).ok_or(Full)?;
        let fixed = uvar_len(head)
            .checked_add(dt.map_or(0, ivar_len))
            .ok_or(Full)?;
        // room for the item's length and at least one byte
        let room = self
            .free()
            .checked_sub(fixed)
            .filter(|r| *r > 0)
            .ok_or(Full)?;
        // the item goes after the longest length the free space can need, then moves down to its real place
        let max_len_len = uvar_len(bytes::u64_of(room));
        let item_at = self
            .state
            .pos
            .checked_add(fixed)
            .and_then(|a| a.checked_add(max_len_len))
            .ok_or(Full)?;
        let end = self.buf.len().checked_sub(CRC_LEN).ok_or(Full)?;
        let space = self.buf.get_mut(item_at..end).ok_or(Full)?;
        let space_len = space.len();
        let len = encode(space).filter(|l| *l <= space_len).ok_or(Full)?;
        let len_len = uvar_len(bytes::u64_of(len));
        let mut pos = self.state.pos;
        let n = varint::put_uvar(self.buf.get_mut(pos..).ok_or(Full)?, head).ok_or(Full)?;
        pos = pos.checked_add(n).ok_or(Full)?;
        if let Some(dt) = dt {
            let n = varint::put_ivar(self.buf.get_mut(pos..).ok_or(Full)?, dt).ok_or(Full)?;
            pos = pos.checked_add(n).ok_or(Full)?;
        }
        // pos + len_len <= item_at: the length takes at most max_len_len bytes
        let real_item_at = pos
            .checked_add(len_len)
            .filter(|a| *a <= item_at)
            .ok_or(Full)?;
        let item_end = item_at.checked_add(len).ok_or(Full)?;
        self.buf.get(item_at..item_end).ok_or(Full)?;
        self.buf.copy_within(item_at..item_end, real_item_at);
        let len_at = self.buf.get_mut(pos..real_item_at).ok_or(Full)?;
        varint::put_uvar(len_at, bytes::u64_of(len)).ok_or(Full)?;
        self.state.pos = real_item_at.checked_add(len).ok_or(Full)?;
        self.state.records = records;
        Ok(())
    }

    /// Write the header's length and count and the CRC; returns the block's length (`&buf[..len]` is the block).
    /// Set the header flags (0 when not set), before [Self::finish]: [FLAG_REPEAT] on blocks of put or copied state records.
    pub fn set_flags(&mut self, flags: u16) {
        // new wrote the header: buf holds HEADER_LEN bytes
        _ = bytes::put_u16(self.buf, 6, flags);
    }

    pub fn finish(self) -> usize {
        let pos = self.state.pos;
        let (body, rest) = self.buf.split_at_mut(pos.min(self.buf.len()));
        #[expect(
            clippy::expect_used,
            reason = "new and from_state hold pos + CRC_LEN <= buf.len() and pos <= MAX_BLOCK_LEN; put keeps it"
        )]
        let (end, len, crc_at) = pos
            .checked_add(CRC_LEN)
            .and_then(|end| {
                let len = u32::try_from(end).ok()?;
                Some((end, len, rest.first_chunk_mut::<CRC_LEN>()?))
            })
            .expect("room for the CRC after the records");
        // the header is in body: pos is at least HEADER_LEN
        _ = bytes::put_u32(body, 8, len);
        _ = bytes::put_u32(body, 12, self.state.records);
        *crc_at = CRC.checksum(body).to_le_bytes();
        end
    }
}

fn copy_parts(buf: &mut [u8], parts: &[&[u8]]) -> Option<usize> {
    let mut rest = buf;
    let mut len = 0usize;
    for p in parts {
        let (dst, tail) = rest.split_at_mut_checked(p.len())?;
        dst.copy_from_slice(p);
        rest = tail;
        len = len.checked_add(p.len())?;
    }
    Some(len)
}

/// A [BlockWriter] without its buffer, see [BlockWriter::into_state].
#[derive(Copy, Clone, Debug)]
pub struct BlockState {
    pos: usize,
    body: usize,
    records: u32,
    slots: Slots,
}

impl BlockState {
    pub fn records(&self) -> u32 {
        self.records
    }
}

/// Write the 32-byte header of a block of `kind`, `len` bytes and `count` records with `id` into `buf`: flags and
/// bytes 28..32 zero.
/// None, writing nothing, when `buf` is shorter than the header or `len` is over [MAX_BLOCK_LEN].
#[must_use = "None means no header was written"]
pub(crate) fn put_header(
    buf: &mut [u8],
    kind: BlockKind,
    len: usize,
    count: u32,
    id: BlockId,
) -> Option<()> {
    if len > MAX_BLOCK_LEN {
        return None;
    }
    let len = u32::try_from(len).ok()?;
    let h = buf.first_chunk_mut::<HEADER_LEN>()?;
    h.fill(0);
    bytes::put(h, 0, MAGIC)?;
    bytes::put(h, 4, [VERSION, kind.to_u8()])?;
    bytes::put_u32(h, 8, len)?;
    bytes::put_u32(h, 12, count)?;
    bytes::put_u64(h, 16, id.seq)?;
    bytes::put_u32(h, 24, id.session.get())
}

/// Why a block can't be read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockError {
    /// Fewer bytes than the header or its length says.
    Short {
        needed: usize,
    },
    BadMagic,
    BadLength(u32),
    BadCrc,
    /// The head of a chunk block is bad.
    Chunk(ChunkHeadError),
    /// The head or body of a summary block is bad.
    Summary(SummaryError),
    /// The clock table or the records don't parse up to the end, or there are more or fewer than the header says.
    BadRecords,
    /// The records of a coded body are read from [Block::decode].
    Coded,
    /// A chunk's head was asked of a block that isn't a chunk block this reader reads ([ChunkHead::head_len]).
    NotChunk,
    /// A chunk block marked [FLAG_PAYLOAD_UNCHECKED]: an integrity level this build doesn't handle, not damage.
    PayloadUnchecked,
    /// A piece head of an integrity level (its byte 146, `level`) this build doesn't handle, not damage: what a
    /// `FileBlocks` reports (`FileError::Refused`). [PieceHeadBlock::read](piece::PieceHeadBlock::read) says it as
    /// [IndexError::UnhandledIntegrity] or [IndexError::UnknownIntegrity].
    UnhandledIntegrity {
        level: u8,
    },
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short { needed } => write!(f, "short: {needed} bytes needed"),
            Self::BadMagic => write!(f, "no block magic"),
            Self::BadLength(len) => write!(f, "a block length of {len} is out of range"),
            Self::BadCrc => write!(f, "the block CRC doesn't match"),
            Self::Chunk(e) => write!(f, "chunk head: {e}"),
            Self::Summary(e) => write!(f, "summary block: {e}"),
            Self::BadRecords => write!(f, "the records don't parse as the header says"),
            Self::Coded => write!(f, "a coded body: its records are read by decoding it"),
            Self::NotChunk => write!(f, "not a chunk block this reader reads"),
            Self::PayloadUnchecked => {
                write!(
                    f,
                    "a chunk with unchecked payload: an integrity level this build doesn't handle"
                )
            }
            Self::UnhandledIntegrity { level } => {
                write!(
                    f,
                    "a piece head of integrity level {level}, which this build doesn't handle"
                )
            }
        }
    }
}

impl core::error::Error for BlockError {}

/// A block read from bytes, CRC checked. Records are read in place, see [Block::records].
#[derive(Copy, Clone, Debug)]
pub struct Block<'b> {
    /// The format version it was written with, [VERSION] or a later one.
    pub version: u8,
    pub kind: BlockKind,
    pub seq: u64,
    /// The session the block belongs to, as written: a reader decides whether it is the current one.
    pub session: SessionTag,
    /// Header flags ([FLAG_REPEAT]); bits this reader doesn't know are kept, not read.
    pub flags: u16,
    pub record_count: u32,
    bytes: &'b [u8],
    /// The bytes the block CRC covers: all but the last [CRC_LEN].
    covered: &'b [u8],
    /// The body: between the header and the block CRC.
    body: &'b [u8],
    /// The block CRC as written.
    crc: u32,
    /// False for a chunk block handed out by [Block::salvage] whose payload doesn't match its CRCs.
    intact: bool,
}

/// A record of a [Block], borrowed from it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Record<'b> {
    pub track: RecordTrack,
    /// None for a describe record (track 0).
    pub at: Option<ClockTime>,
    pub item: &'b [u8],
}

/// Length of the block at the start of `bytes` from its header, without checking more than the magic; for readers
/// of a byte stream.
pub fn block_len(bytes: &[u8]) -> Result<usize, BlockError> {
    let short = BlockError::Short { needed: HEADER_LEN };
    let h = bytes.first_chunk::<HEADER_LEN>().ok_or(short)?;
    if bytes::array::<4>(h, 0) != Some(MAGIC) {
        return Err(BlockError::BadMagic);
    }
    let raw = bytes::le_u32(h, 8).ok_or(short)?;
    FromFile::new(raw)
        .at_most(MAX_BLOCK_LEN)
        .ok()
        .filter(|len| *len >= HEADER_LEN + CRC_LEN)
        .ok_or(BlockError::BadLength(raw))
}

/// Give the whole, good block `block` the session and seq of `id` and rewrite its CRCs.
///
/// A chunk or index block's head CRC is rewritten too. A writer that places blocks later than it made them (a summary
/// block filling the gap before an aligned chunk) so keeps seqs ascending in the file and every block in its session.
pub fn set_id(block: &mut [u8], id: BlockId) -> Result<(), BlockError> {
    restamp(block, id, 0, Reread::Whole)
}

/// [set_id] of a block whose CRCs are known good, read whole once already: only its header and a chunk's head are
/// read again, no pass over its payload or records.
#[cfg(feature = "alloc")]
pub(crate) fn set_id_trusted(block: &mut [u8], id: BlockId) -> Result<(), BlockError> {
    restamp(block, id, 0, Reread::Head)
}

/// [set_id], and mark the block as one of put or copied state records ([FLAG_REPEAT]).
pub fn mark_repeat(block: &mut [u8], id: BlockId) -> Result<(), BlockError> {
    restamp(block, id, FLAG_REPEAT, Reread::Whole)
}

/// Mark the whole, good events chunk block `block` as a break in its track's monotone relations.
///
/// The mark is [FLAG_RELATION_BREAK], and the CRCs are rewritten. It serves a caller's block given to a writer that
/// waits for a break ([TapeWriter::accept_relation_break](crate::TapeWriter::accept_relation_break)).
///
/// Another kind of block: [BlockError::NotChunk].
pub fn mark_relation_break(block: &mut [u8]) -> Result<(), BlockError> {
    let b = Block::read(block)?;
    if b.kind != BlockKind::Events {
        return Err(BlockError::NotChunk);
    }
    let id = BlockId::new(b.session, b.seq);
    restamp(block, id, FLAG_RELATION_BREAK, Reread::Whole)
}

/// Mark the whole, good block `block` as belonging with the block after it ([FLAG_WITH_NEXT]), its CRCs rewritten.
#[cfg(feature = "alloc")]
pub(crate) fn mark_with_next(block: &mut [u8]) -> Result<(), BlockError> {
    let b = Block::read(block)?;
    let id = BlockId::new(b.session, b.seq);
    restamp(block, id, FLAG_WITH_NEXT, Reread::Whole)
}

/// How [restamp] reads the block it is given.
#[derive(Copy, Clone)]
enum Reread {
    /// Whole, every CRC checked ([Block::read]).
    Whole,
    /// Its header and a chunk's head only, its CRCs known good ([Block::read_trusted]); only the writer has such.
    #[cfg(feature = "alloc")]
    Head,
}

/// Give the good block `block`, read as `read` says, the id `id` and the header flags `set` too, its CRCs rewritten.
fn restamp(block: &mut [u8], id: BlockId, set: u16, read: Reread) -> Result<(), BlockError> {
    let b = match read {
        Reread::Whole => Block::read(block)?,
        #[cfg(feature = "alloc")]
        Reread::Head => Block::read_trusted(block)?,
    };
    if b.len() != block.len() {
        return Err(BlockError::BadLength(
            u32::try_from(block.len()).unwrap_or(u32::MAX),
        ));
    }
    let chunk = b.is_chunk() && b.body_known();
    let index = b.kind == BlockKind::Index && b.body_known();
    let flags = b.flags | set;
    // a block read whole holds its header and CRC
    let short = BlockError::Short {
        needed: HEADER_LEN + CRC_LEN,
    };
    bytes::put_u16(block, 6, flags).ok_or(short)?;
    bytes::put_u64(block, 16, id.seq).ok_or(short)?;
    bytes::put_u32(block, 24, id.session.get()).ok_or(short)?;
    if chunk {
        chunk::restamp(block).map_err(BlockError::Chunk)?;
    } else {
        if index {
            index::restamp_head(block);
        }
        let (covered, crc) = block.split_last_chunk_mut::<CRC_LEN>().ok_or(short)?;
        *crc = CRC.checksum(covered).to_le_bytes();
    }
    Ok(())
}

/// The clock table at the start of a body: the slots and where the records start in `body`.
fn read_clock_table(body: &[u8]) -> Option<(Slots, usize)> {
    let mut rd = varint::Reader(body);
    let n = rd.uvar_file()?.at_most(MAX_BLOCK_CLOCKS).ok()?;
    let mut slots = Slots::NONE;
    slots.n = u8::try_from(n).ok()?;
    // strictly ascending from 0: no clock is 0 and none repeats
    let mut prev = 0;
    for slot in slots.clocks.iter_mut().take(n) {
        let c = u32::try_from(rd.uvar()?).ok()?;
        if c <= prev {
            return None;
        }
        *slot = ClockNum::try_new(c)?;
        prev = c;
    }
    Some((slots, body.len().checked_sub(rd.0.len())?))
}

impl<'b> Block<'b> {
    /// Read the block at the start of `bytes`; bytes after it are not looked at ([Block::len] says where it ends).
    ///
    /// A chunk block is checked by its head CRC, its segment CRCs and its block CRC, in one pass over the payload.
    pub fn read(bytes: &'b [u8]) -> Result<Self, BlockError> {
        let (block, chunk) = Self::read_unchecked(bytes)?;
        block.check_integrity(chunk.is_some())?;
        match chunk {
            Some(c) if c.check_payload() => Ok(block),
            Some(_) => Err(BlockError::BadCrc),
            None => {
                if CRC.checksum(block.covered) != block.crc {
                    return Err(BlockError::BadCrc);
                }
                block.check_records()?;
                if block.is_summary() {
                    SummaryHead::read(block.bytes).map_err(BlockError::Summary)?;
                }
                Ok(block)
            }
        }
    }

    /// Read a block as [Self::read] does, but take a chunk block whose payload is damaged as long as its head is good
    /// (head CRC, ranges): [Self::is_intact] is then false and [Chunk::bad_segments] says what is lost.
    pub fn salvage(bytes: &'b [u8]) -> Result<Self, BlockError> {
        let (mut block, chunk) = Self::read_unchecked(bytes)?;
        block.check_integrity(chunk.is_some())?;
        match chunk {
            Some(c) => {
                block.intact = c.check_payload();
                Ok(block)
            }
            None => Self::read(bytes),
        }
    }

    /// The block's header, with no bytes: [Block::on] lends it again on the bytes it was read from, without a
    /// second check.
    #[cfg(feature = "alloc")]
    pub(crate) fn detach(&self) -> Block<'static> {
        Block {
            version: self.version,
            kind: self.kind,
            seq: self.seq,
            session: self.session,
            flags: self.flags,
            record_count: self.record_count,
            bytes: &[],
            covered: &[],
            body: &[],
            crc: self.crc,
            intact: self.intact,
        }
    }

    /// A [Block::detach]ed header on `bytes`, the whole block it was read from; None when `bytes` is too short for
    /// a block.
    #[cfg(feature = "alloc")]
    pub(crate) fn on<'c>(&self, bytes: &'c [u8]) -> Option<Block<'c>> {
        let (covered, _) = bytes.split_last_chunk::<CRC_LEN>()?;
        let (_, body) = covered.split_first_chunk::<HEADER_LEN>()?;
        Some(Block {
            version: self.version,
            kind: self.kind,
            seq: self.seq,
            session: self.session,
            flags: self.flags,
            record_count: self.record_count,
            bytes,
            covered,
            body,
            crc: self.crc,
            intact: self.intact,
        })
    }

    #[cfg(feature = "alloc")]
    /// The block of a whole block whose CRCs are known good (one this crate made, or read with [Self::read] already):
    /// the header, and for a chunk block its head with its head CRC, read; no pass over the payload or the records.
    pub(crate) fn read_trusted(bytes: &'b [u8]) -> Result<Self, BlockError> {
        Self::read_unchecked(bytes).map(|(b, _)| b)
    }

    /// Refuse a chunk block marked [FLAG_PAYLOAD_UNCHECKED]: this build handles full integrity only.
    fn check_integrity(&self, chunk: bool) -> Result<(), BlockError> {
        match chunk && self.flags & FLAG_PAYLOAD_UNCHECKED != 0 {
            true => Err(BlockError::PayloadUnchecked),
            false => Ok(()),
        }
    }

    /// The header, and for a chunk block its head checked; no CRC over the payload yet.
    fn read_unchecked(bytes: &'b [u8]) -> Result<(Self, Option<Chunk<'b>>), BlockError> {
        let len = block_len(bytes)?;
        let short = BlockError::Short { needed: len };
        let (whole, _) = bytes.split_at_checked(len).ok_or(short)?;
        // block_len: len is at least HEADER_LEN + CRC_LEN
        let (covered, crc) = whole.split_last_chunk::<CRC_LEN>().ok_or(short)?;
        let (h, body) = covered.split_first_chunk::<HEADER_LEN>().ok_or(short)?;
        let block = Block {
            version: h[4],
            kind: BlockKind::from_u8(h[5]),
            record_count: bytes::le_u32(h, 12).ok_or(short)?,
            seq: bytes::le_u64(h, 16).ok_or(short)?,
            flags: bytes::le_u16(h, 6).ok_or(short)?,
            session: SessionTag::from_u32(bytes::le_u32(h, 24).ok_or(short)?),
            bytes: whole,
            covered,
            body,
            crc: u32::from_le_bytes(*crc),
            intact: true,
        };
        if block.body_known() && block.is_chunk() {
            let chunk = Chunk::read_head(block.bytes).map_err(BlockError::Chunk)?;
            return Ok((block, Some(chunk)));
        }
        Ok((block, None))
    }

    /// The records parse to the end and are as many as the header says.
    fn check_records(&self) -> Result<(), BlockError> {
        let records = self.kind == BlockKind::Records;
        if self.version == VERSION && records && self.is_coded() && self.coded().is_none() {
            return Err(BlockError::BadRecords);
        }
        if !self.has_records() || self.plain_body().is_none() {
            return Ok(());
        }
        if self.clock_table().is_none() {
            return Err(BlockError::BadRecords);
        }
        // a record takes 2 bytes at least: no more of them than body bytes
        let mut n = 0u32;
        for r in self.records() {
            r?;
            n = n.checked_add(1).ok_or(BlockError::BadRecords)?;
        }
        if n != self.record_count {
            return Err(BlockError::BadRecords);
        }
        Ok(())
    }

    /// Whether the block holds put or copied state records ([FLAG_REPEAT]).
    pub fn is_repeat(&self) -> bool {
        self.flags & FLAG_REPEAT != 0
    }

    /// Whether the block is an events chunk whose track's monotone relations start again ([FLAG_RELATION_BREAK]).
    pub fn is_relation_break(&self) -> bool {
        self.kind == BlockKind::Events && self.flags & FLAG_RELATION_BREAK != 0
    }

    /// The track of its first record, None for a block without records this reader reads in place: what tells a
    /// block of definitions (track 0) from a block of data.
    pub fn first_track(&self) -> Option<RecordTrack> {
        self.records().next()?.ok().map(|r| r.track)
    }

    /// The block's CRC, the last 4 bytes: with its length, what tells two blocks of one id apart.
    pub fn crc(&self) -> u32 {
        self.crc
    }

    /// Whether the block is good throughout. Only [Self::salvage] hands out one that isn't.
    pub fn is_intact(&self) -> bool {
        self.intact
    }

    fn is_chunk(&self) -> bool {
        matches!(self.kind, BlockKind::Events | BlockKind::Series)
    }

    fn is_summary(&self) -> bool {
        self.body_known() && self.kind == BlockKind::Summary
    }

    /// The head of a summary block whose body this reader knows: what an index lists, without the nodes.
    pub fn summary_head(&self) -> Option<SummaryHead> {
        if !self.is_summary() {
            return None;
        }
        SummaryHead::read(self.bytes).ok().map(|(head, _, _)| head)
    }

    fn has_records(&self) -> bool {
        self.body_known() && self.kind == BlockKind::Records
    }

    /// The chunk of an events or series block whose body this reader knows.
    pub fn chunk(&self) -> Option<Chunk<'b>> {
        if !(self.body_known() && self.is_chunk()) {
            return None;
        }
        Chunk::read_head(self.bytes).ok()
    }

    /// The whole block, header and CRC included.
    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.record_count == 0
    }

    /// The clocks of its records, ascending; none for a block whose body this reader doesn't know.
    pub fn clocks(&self) -> impl Iterator<Item = ClockNum> {
        self.clock_table()
            .into_iter()
            .flat_map(|(s, _)| s.clocks.into_iter().take(s.len()))
    }

    /// The clock table and where the records start in the block; None when this reader doesn't know the body, the
    /// body is coded other than [stored](BodyCodec::STORED) or the table doesn't parse.
    fn clock_table(&self) -> Option<(Slots, usize)> {
        if !self.has_records() {
            return None;
        }
        let (body, at) = self.plain_body()?;
        let (slots, table) = read_clock_table(body)?;
        Some((slots, at.checked_add(table)?))
    }

    /// The body as stored and where it starts in the block: None for a body coded other than stored.
    fn plain_body(&self) -> Option<(&'b [u8], usize)> {
        if !self.is_coded() {
            return Some((self.body, HEADER_LEN));
        }
        let c = body::coded(self.body)?;
        if c.codec != BodyCodec::STORED {
            return None;
        }
        // a stored body ends where the block's body ends
        Some((c.bytes, self.covered.len().checked_sub(c.bytes.len())?))
    }

    fn is_coded(&self) -> bool {
        self.flags & FLAG_BODY_CODED != 0
    }

    /// The parts of a coded record body; None for any other block, or one that doesn't parse.
    fn coded(&self) -> Option<body::Coded<'b>> {
        let records = self.kind == BlockKind::Records;
        if !(self.is_coded() && records) {
            return None;
        }
        body::coded(self.body)
    }

    /// The length of the plain block [Self::decode] makes of a body coded with lz4, from the body's head; None for
    /// any other block.
    #[cfg(feature = "alloc")]
    pub(crate) fn decoded_len(&self) -> Option<usize> {
        self.coded()
            .filter(|c| c.codec == BodyCodec::LZ4 && self.body_known())
            .map(|c| c.decoded_len.saturating_add(HEADER_LEN + CRC_LEN))
    }

    /// The codec of a coded record body ([FLAG_BODY_CODED]); None for a plain body.
    pub fn body_codec(&self) -> Option<BodyCodec> {
        self.coded().map(|c| c.codec)
    }

    /// The block with its records readable in place: itself, unless its body is coded with lz4, which is decoded
    /// into `buf` as a plain block (the flag cleared, its length and CRC new; the block's place in a file is the
    /// coded one's).
    #[cfg(feature = "alloc")]
    pub fn decode<'d>(&self, buf: &'d mut alloc::vec::Vec<u8>) -> Result<Block<'d>, BlockError>
    where
        'b: 'd,
    {
        match self.coded() {
            Some(c) if c.codec == BodyCodec::LZ4 && self.body_known() => {
                body::decode_into(self, &c, buf)
            }
            _ => Ok(*self),
        }
    }

    /// Whether this reader knows the block's body: its version and kind, and for a coded body the codec. A block it
    /// doesn't know has no [records](Self::records) for it and is skipped.
    pub fn body_known(&self) -> bool {
        self.version == VERSION
            && !matches!(self.kind, BlockKind::Other(_) | BlockKind::Parity)
            && (!self.is_coded() || self.coded().is_some_and(|c| c.codec.is_known()))
    }

    /// The records, none for a chunk block or a block whose body this reader doesn't know ([Self::body_known]). A
    /// body coded with lz4 gives [BlockError::Coded]: read the records of [Self::decode].
    pub fn records(&self) -> Records<'b> {
        let (slots, body) = self
            .clock_table()
            .and_then(|(slots, at)| Some((slots, self.covered.get(at..)?)))
            .unwrap_or((Slots::NONE, &[]));
        Records {
            body,
            slots,
            failed: false,
            coded: self.has_records() && self.plain_body().is_none(),
        }
    }
}

/// Iterator over the records of a block, see [Block::records].
pub struct Records<'b> {
    body: &'b [u8],
    slots: Slots,
    failed: bool,
    /// The body is coded: one [BlockError::Coded] and no records.
    coded: bool,
}

impl<'b> Records<'b> {
    fn read_one(&mut self) -> Option<Record<'b>> {
        let mut rd = varint::Reader(self.body);
        let head = rd.uvar()?;
        // bits is at most 4 (16 slots): the shifts stay in range
        let bits = self.slots.bits();
        let track = RecordTrack::from_u32(u32::try_from(head >> bits).ok()?);
        let slot = usize::try_from(head & ((1u64 << bits).wrapping_sub(1))).ok()?;
        let at = if track.is_definitions() {
            if slot != 0 {
                return None;
            }
            None
        } else {
            if slot >= self.slots.len() {
                return None;
            }
            let clock = *self.slots.clocks.get(slot)?;
            let last = self.slots.last.get_mut(slot)?;
            let ticks = last.wrapping_add(rd.ivar()?);
            *last = ticks;
            Some(ClockTime::new(clock, ticks))
        };
        let len = rd.uvar_file()?.within(rd.0.len()).ok()?;
        let item = rd.take(len)?;
        self.body = rd.0;
        Some(Record { track, at, item })
    }
}

impl<'b> Iterator for Records<'b> {
    type Item = Result<Record<'b>, BlockError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.coded {
            self.coded = false;
            self.failed = true;
            return Some(Err(BlockError::Coded));
        }
        if self.body.is_empty() || self.failed {
            return None;
        }
        match self.read_one() {
            Some(r) => Some(Ok(r)),
            None => {
                self.failed = true;
                Some(Err(BlockError::BadRecords))
            }
        }
    }
}

#[cfg(test)]
mod tests;
