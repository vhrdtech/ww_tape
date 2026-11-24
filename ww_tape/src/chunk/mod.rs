//! Chunk blocks: one chunk of one track, its columns read in place.
//!
//! An events chunk ([BlockKind::Events]) holds up to a few thousand items of an event track, a series chunk
//! ([BlockKind::Series]) a run of samples of a fixed-rate series. The block header's count (bytes 12..16) is the
//! chunk's item or sample count; bytes 28..32 hold a CRC of the head, and the body starts with where the payload
//! starts:
//!
//! ```text
//! off size field
//!  24 u32  session tag, as in every block
//!  28 u32  head CRC: CRC-32C over bytes 0..28, then 32..payload offset
//!  32 u32  payload offset from the block start (in an aligned file a multiple of the alignment in the file)
//!  36 u32  track number (TrackDef::number)
//!  40 u64  track id, the identity hash of the track (TrackDef::id): the chunk places itself without the definitions
//!  48 u64  first item number of the track (sample index in a series)
//!  56 i64  first tick: the first item's start (the first sample's tick)
//!  64 i64  last tick: the last item's start (the last sample's tick); at least the first
//!  72 i64  latest end: the largest end of any item, at least the last tick; the last tick when every item is an
//!          instant (EndMode::Absent) and in a series chunk. An overlap query and a rebuilt index need no decode of the
//!          ends column.
//!  80  u8  segment shift: the payload is checked in segments of 2^shift bytes (16: 64 KiB)
//!  81  u8  columns n, 1 and up
//!  82  u8  mode: an events chunk's time mode, the time codec's; a series chunk's fixed axes a, 0 to
//!          GRID_FIXED_AXES_MAX (0: a series of no fixed axes, GridDef)
//!  83  u8  end byte: an events chunk's end mode (EndMode); a series chunk's overview level k, 0 for the cells
//!          as written, 1 to GRID_OVERVIEW_LEVELS_MAX for space overview k of a grid of fixed axes
//!  84 ...  series chunks only, the grid of the samples (SeriesGrid):
//!          84 u64  grid index: the sample the grid counts from, at most the first item
//!          92 i64  grid tick: the tick of that sample; sample i is at grid tick + (i - grid index) · period,
//!                  rounded down, the period the track's GridDef gives
//!         100  u8  scale length s, 1 to SCALE_LEN_MAX
//!         101 ...  scale: what a stored number is worth in the sample type's unit, as the shrink_wrap bytes of an
//!                  exact_units Magnitude (10⁻³ is fa 00)
//!             ...  a × u32 tile coordinate: the chunk's tile along each fixed axis, its first cell over the axis'
//!                  tile extent (TileCoords; none for a series)
//!     ...  n × column: kind u8, codec u8, param u16, offset u32 (from the payload start; the first 0, ascending)
//!     ...  segment CRCs: one u32 per segment of the payload, CRC-32C of its bytes
//!     ...  zeros up to the payload offset
//!  payload: the columns back to back, then 16 zero bytes, so an unaligned 16-byte load at any column byte stays in
//!           the block
//! len-4 u32 CRC over bytes 0..len-4, as in every block
//! ```
//!
//! A series chunk's head carries its grid and scale ([SeriesGrid]), so the chunk times and scales its samples
//! alone: a reader needs the track's period from its definition and nothing else ([series](crate::series#grids)).
//!
//! A column runs from its offset to the next one's (the last to the padding). The head CRC lets a reader trust the
//! head, and so the block length, the chunk's place and the segment CRCs, when the payload is damaged: a reader in
//! [Recovery::Resync](crate::Recovery) skips such a block by its length and hands it out with the bad segments
//! marked ([Chunk::bad_segments]), so a flipped bit in a 4 MiB series chunk costs one 64 KiB segment. Event chunks are
//! usually a single segment; a codec may still salvage items from it by checking its own group headers against
//! [Chunk::column_damaged].
//!
//! An overview chunk (byte 83 not 0, [grid](crate::grid#overviews)) holds one [ColumnKind::OverviewMin],
//! [ColumnKind::OverviewMax] and [ColumnKind::OverviewMean] column each, coded as samples columns, and no
//! [ColumnKind::Samples] column; any other chunk holds none of the three.
//!
//! Codec ids name how a column is coded: [Codec::RAW] is the column as plain little-endian values or bytes, ids 1 to
//! 3 are the event time codec's ([crate::times]), 4 payload lengths ([crate::payloads]), 5 a series chunk's flag
//! runs ([crate::flag_runs]), 16 to 47 the sample codecs'
//! ([crate::samples]), 48 lz4 payloads ([crate::lz4]), 49 [Codec::SERIES] reserved.
//!
//! A reader refuses a column whose kind or codec it doesn't know, and still reads the chunk's other columns: each
//! reader ([EventTimes](crate::times::EventTimes), [Payloads](crate::payloads::Payloads),
//! [Samples](crate::samples::Samples)) takes the columns of its kinds and leaves the rest. Kind 7 is reserved
//! ([ColumnKind::PAYLOAD_LEAF]).
//!
//! **Reserved, not built:** [Codec::SERIES] (49) is a [ColumnKind::Payloads] column holding one shrink_wrap
//! `Series<T>` of the chunk's items, so items of a fixed-size struct are stored as columns of their fields. Such a
//! column stands alone: a [ColumnKind::PayloadLengths] column beside it is an error, the chunk holds at most 65535
//! items and the series' count is the head's count. Until it is built, [Payloads](crate::payloads::Payloads)
//! refuses it as a codec it doesn't know.
//!
//! Bit 7 of the codec byte ([Codec::OUTER]) marks an outer-coded column: its bytes are an outer codec u8, the decoded
//! length (uvar) and the outer-coded bytes, and the inner codec (bits 0..6, [Codec::inner]) applies to the decoded
//! bytes. An outer-coded column is not read in place nor salvaged by segment, so it suits payload columns and
//! archived pieces, never a time column. No outer codec is assigned yet: a reader refuses every outer-coded column as
//! a column in a codec it doesn't know. Inner codec ids stop at 127.

use crate::{ItemNum, TrackId};
use core::num::NonZeroUsize;

use crate::TrackNum;
use crate::bytes::{file_u8, file_u32, le_i64, le_u8, le_u16, le_u32, le_u64};
use crate::grid::{GRID_FIXED_AXES_MAX, GRID_OVERVIEW_LEVELS_MAX, TileCoords};
use crate::series::{SCALE_LEN_MAX, SeriesGrid, read_scale, scale_bytes};
use crate::{
    BlockError, BlockKind, BoundError, CRC_LEN, FLAG_BODY_CODED, HEADER_LEN, MAX_BLOCK_LEN, VERSION,
};

// The CRC of chunk blocks: the same CRC-32C as every block.
use crate::CRC;

/// Bytes of the fixed part of the chunk head (32..84).
pub const CHUNK_FIXED_LEN: usize = 52;
/// Bytes of the grid of a series chunk before its scale: grid index, grid tick, scale length (84..101).
pub const SERIES_GRID_LEN: usize = 17;
/// Bytes of one column entry.
pub const COLUMN_ENTRY_LEN: usize = 8;
/// Zero bytes after the columns.
pub const PAYLOAD_PADDING: usize = 16;
/// Segment shift writers use: 64 KiB segments.
pub const SEGMENT_SHIFT: u8 = 16;
/// Smallest segment shift a reader takes (4 KiB segments; the largest, 16 MiB, is [SEGMENT_SHIFT_MAX]).
pub const SEGMENT_SHIFT_MIN: u8 = 12;
/// Largest segment shift a reader takes.
pub const SEGMENT_SHIFT_MAX: u8 = 24;
/// Most segments a chunk can have: [MAX_BLOCK_LEN] in segments of 2^[SEGMENT_SHIFT_MIN].
pub const MAX_SEGMENTS: usize = MAX_BLOCK_LEN >> SEGMENT_SHIFT_MIN;
/// Most columns in a chunk: the column count is a byte.
pub const MAX_COLUMNS: usize = 255;

/// Bytes of the head before the column entries: the block header and the fixed part.
const FIXED_END: usize = HEADER_LEN + CHUNK_FIXED_LEN;
/// Where a series chunk's grid tick is: after the grid index.
const GRID_TICK_AT: usize = FIXED_END + 8;
/// Where a series chunk's scale length is: after the grid index and tick.
const GRID_SCALE_LEN_AT: usize = FIXED_END + 16;
/// Where a series chunk's scale starts.
const GRID_SCALE_AT: usize = FIXED_END + SERIES_GRID_LEN;
/// Where a chunk's mode byte is: an events chunk's time mode, a series chunk's count of fixed axes.
const AXES_AT: usize = 82;
/// Where a chunk's end byte is: an events chunk's end mode, a series chunk's overview level.
const END_AT: usize = 83;

/// Bytes of the tile coordinates of `axes` fixed axes; `axes` is at most [GRID_FIXED_AXES_MAX].
const fn tile_coords_len(axes: usize) -> usize {
    axes.saturating_mul(4)
}

/// Byte 82 of `head`: the time mode of an events chunk, the count of fixed axes of a series chunk.
fn mode_byte(head: &ChunkHead) -> Result<u8, ChunkError> {
    match head.kind {
        BlockKind::Series => u8::try_from(head.tile.len())
            .ok()
            .ok_or(ChunkError::Invalid(
                "more fixed axes than GRID_FIXED_AXES_MAX",
            )),
        _ => Ok(head.time_mode),
    }
}
/// Byte 83 of `head`: the end mode of an events chunk, the overview level of a series chunk.
fn end_byte(head: &ChunkHead) -> u8 {
    match head.kind {
        BlockKind::Series => head.level,
        _ => head.end_mode.to_u8(),
    }
}
/// Bytes of a segment writers use, 2^[SEGMENT_SHIFT].
const SEGMENT_BYTES: usize = 1 << SEGMENT_SHIFT;
/// Bytes of a segment CRC.
const SEGMENT_CRC_LEN: usize = 4;

const _: () = assert!(
    usize::BITS >= u32::BITS && usize::BITS <= u64::BITS,
    "usize is 32 to 64 bits: a u32 fits it, it fits a u64"
);

/// `n` as a u64.
pub(crate) const fn len_u64(n: usize) -> u64 {
    #[expect(
        clippy::as_conversions,
        reason = "usize is at most 64 bits (the const assert above); From is not const"
    )]
    let v = n as u64;
    v
}

/// `n` as a usize.
pub(crate) const fn u32_len(n: u32) -> usize {
    #[expect(
        clippy::as_conversions,
        reason = "usize is at least 32 bits (the const assert above); From is not const"
    )]
    let v = n as usize;
    v
}

pub use ww_tape_codecs::Codec;

/// Most items of a chunk whose payloads column is in [Codec::SERIES].
pub const SERIES_ITEMS_MAX: u32 = 65_535;

/// What a column holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnKind {
    /// Item start ticks (events).
    Starts,
    /// Item ends, as the end mode says (events).
    Ends,
    /// The items' flags: a u32 word per item (events), or runs of samples sharing a word (series,
    /// [Codec::FLAG_RUNS], [Chunk::flag_runs]).
    Flags,
    /// Length of each item's payload (events).
    PayloadLengths,
    /// Item payloads back to back (events).
    Payloads,
    /// Samples of one channel, or of the channels a codec interleaves; the param names the (first) channel (series).
    Samples,
    /// One relation's values, a u64 per item; the param names the relation by its index in
    /// [TrackDef::relations](crate::TrackDef::relations) (events, [RelationColumn](crate::times::RelationColumn)).
    Relation,
    /// The smallest value of each overview cell's source cells, per channel, laid out and coded as
    /// [ColumnKind::Samples]; param 0 (a series chunk of an overview level, [Overview](crate::grid::Overview)).
    OverviewMin,
    /// The largest value of each overview cell's source cells, as [ColumnKind::OverviewMin].
    OverviewMax,
    /// The mean of each overview cell's source cells, as [ColumnKind::OverviewMin]: rounded to the nearest integer,
    /// ties to even, for an integer element.
    OverviewMean,
    /// A kind this reader doesn't know.
    Other(u8),
}

/// A column kind as error texts name it ([ColumnKind::named]).
#[derive(Copy, Clone, Debug)]
pub(crate) struct ColumnName(ColumnKind);

impl core::fmt::Display for ColumnName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            ColumnKind::Starts => f.write_str("starts"),
            ColumnKind::Ends => f.write_str("ends"),
            ColumnKind::Flags => f.write_str("flags"),
            ColumnKind::PayloadLengths => f.write_str("payload lengths"),
            ColumnKind::Payloads => f.write_str("payloads"),
            ColumnKind::Samples => f.write_str("samples"),
            ColumnKind::Relation => f.write_str("relation"),
            ColumnKind::OverviewMin => f.write_str("overview min"),
            ColumnKind::OverviewMax => f.write_str("overview max"),
            ColumnKind::OverviewMean => f.write_str("overview mean"),
            ColumnKind::Other(k) => write!(f, "kind {k}"),
        }
    }
}

impl ColumnKind {
    /// Reserved, kind 7: one leaf of the items' payload struct, its param the leaf's index. Read as
    /// [ColumnKind::Other] until it is built, so a reader refuses it.
    pub const PAYLOAD_LEAF: Self = Self::Other(7);

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Starts,
            2 => Self::Ends,
            3 => Self::Flags,
            4 => Self::PayloadLengths,
            5 => Self::Payloads,
            6 => Self::Samples,
            8 => Self::Relation,
            9 => Self::OverviewMin,
            10 => Self::OverviewMax,
            11 => Self::OverviewMean,
            other => Self::Other(other),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Starts => 1,
            Self::Ends => 2,
            Self::Flags => 3,
            Self::PayloadLengths => 4,
            Self::Payloads => 5,
            Self::Samples => 6,
            Self::Relation => 8,
            Self::OverviewMin => 9,
            Self::OverviewMax => 10,
            Self::OverviewMean => 11,
            Self::Other(v) => v,
        }
    }

    /// The kind as an error text names it: `starts`, `payload lengths`, `kind 9`.
    pub(crate) fn named(self) -> ColumnName {
        ColumnName(self)
    }

    /// Whether the kind belongs in a chunk of `block` kind; unknown kinds belong anywhere.
    fn fits(self, block: BlockKind) -> bool {
        match self {
            Self::Samples | Self::OverviewMin | Self::OverviewMax | Self::OverviewMean => {
                block == BlockKind::Series
            }
            // per item in an events chunk, as flag runs in a series chunk
            Self::Flags | Self::Other(_) => true,
            _ => block == BlockKind::Events,
        }
    }
}

/// How an events chunk stores ends.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum EndMode {
    /// Every item is an instant: no ends column.
    Absent,
    /// The end of item i is the start of item i + 1, the last item's end is the head's latest end: no ends column.
    NextStart,
    /// The ends column holds each item's duration.
    Durations,
}

impl EndMode {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Absent),
            1 => Some(Self::NextStart),
            2 => Some(Self::Durations),
            _ => None,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Absent => 0,
            Self::NextStart => 1,
            Self::Durations => 2,
        }
    }
}

/// What a chunk is: which track, which items, which ticks. The fixed part of the head.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChunkHead {
    /// [BlockKind::Events] or [BlockKind::Series].
    pub kind: BlockKind,
    /// [TrackDef::id](crate::TrackDef::id).
    pub track_id: TrackId,
    /// [TrackDef::number](crate::TrackDef::number).
    pub track_number: TrackNum,
    /// Item number of the first item in the track (sample index in a series).
    pub first_item: ItemNum,
    /// Items (samples) in the chunk, 1 and up.
    pub count: u32,
    pub first_tick: i64,
    pub last_tick: i64,
    /// The largest end of any item, at least [Self::last_tick]; equal to it when every item is an instant
    /// ([EndMode::Absent]) and in a series chunk.
    pub latest_end: i64,
    /// The time codec's mode (events), 0 in a series chunk.
    pub time_mode: u8,
    /// [EndMode] (events); [EndMode::Absent] in a series chunk.
    pub end_mode: EndMode,
    /// The grid and scale of a series chunk's samples; None in an events chunk.
    pub grid: Option<SeriesGrid>,
    /// The tile a series chunk of a grid holds, one coordinate per fixed axis ([grid](crate::grid#tiles)); none in a
    /// series of no fixed axes and in an events chunk.
    pub tile: TileCoords,
    /// The overview level of a grid chunk: 0 for the cells as written, k from 1 to [GRID_OVERVIEW_LEVELS_MAX] for the
    /// space overview whose downsampled axes are halved k times ([grid](crate::grid#overviews)); 0 in an events chunk
    /// and a series of no fixed axes.
    pub level: u8,
}

/// Bytes of a chunk block [ChunkHead::head_len] reads: the block header and the payload offset.
pub const CHUNK_HEAD_PREFIX: usize = HEADER_LEN + 4;

impl ChunkHead {
    /// Bytes of the head of the chunk block whose first [CHUNK_HEAD_PREFIX] bytes are `header`: what
    /// [Self::read_head] needs, the bytes up to the payload. A block of another kind or format version, or with a
    /// coded body, is [BlockError::NotChunk].
    pub fn head_len(header: &[u8]) -> Result<usize, BlockError> {
        let len = crate::block_len(header)?;
        if header.len() < CHUNK_HEAD_PREFIX {
            return Err(BlockError::Short {
                needed: CHUNK_HEAD_PREFIX,
            });
        }
        let kind = le_u8(header, 5).map(BlockKind::from_u8);
        let flags = le_u16(header, 6).ok_or(BlockError::NotChunk)?;
        if le_u8(header, 4) != Some(VERSION)
            || !matches!(kind, Some(BlockKind::Events | BlockKind::Series))
            || flags & FLAG_BODY_CODED != 0
        {
            return Err(BlockError::NotChunk);
        }
        payload_offset(header, len).map_err(BlockError::Chunk)
    }

    /// Read the head of a chunk block from `bytes`, its first [Self::head_len] bytes or more: head CRC, ranges,
    /// column entries, as [Block::read](crate::Block::read) checks them. The payload is neither read nor checked:
    /// for a series chunk, its grid and scale come without its samples.
    pub fn read_head(bytes: &[u8]) -> Result<Self, BlockError> {
        let payload_at = Self::head_len(bytes)?;
        let len = crate::block_len(bytes)?;
        let head = bytes
            .get(..payload_at)
            .ok_or(BlockError::Short { needed: payload_at })?;
        HeadBytes::read(head, len)
            .map(|h| h.head)
            .map_err(BlockError::Chunk)
    }
}

/// A column to write: what it is and how long its bytes are.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    pub kind: ColumnKind,
    pub codec: Codec,
    /// The channel of a [ColumnKind::Samples] column, else 0.
    pub param: u16,
    pub len: usize,
}

/// A column of a chunk read from a block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Column<'b> {
    pub kind: ColumnKind,
    pub codec: Codec,
    pub param: u16,
    /// Offset of the column in the payload.
    pub offset: usize,
    /// The column's bytes; the 16 bytes after them are still in the block.
    pub bytes: &'b [u8],
}

/// Why a chunk can't be written.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChunkError {
    /// The buffer is shorter than the block ([chunk_len]).
    Full { needed: usize },
    /// The block would be longer than [MAX_BLOCK_LEN].
    TooLong { len: usize },
    /// The head or the columns break a rule of the format (no columns, count 0, last tick before the first, a column
    /// of another chunk kind, the kind not a chunk kind).
    Invalid(&'static str),
    /// The chunk's scale doesn't encode.
    ///
    /// The variant keeps the encoder's error; [source](core::error::Error::source) gives it only with `std`, where
    /// shrink_wrap's error implements the trait.
    Scale(shrink_wrap::Error),
}

impl core::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full { needed } => write!(f, "the buffer is short: {needed} bytes needed"),
            Self::TooLong { len } => {
                write!(f, "a block of {len} bytes is over {MAX_BLOCK_LEN}")
            }
            Self::Invalid(what) => write!(f, "{what}"),
            Self::Scale(_) => write!(f, "a scale that doesn't encode"),
        }
    }
}

impl core::error::Error for ChunkError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            #[cfg(feature = "std")]
            Self::Scale(e) => Some(e),
            _ => None,
        }
    }
}

/// Bytes of the head (block header included) of a chunk, before any alignment padding.
///
/// The chunk has `head`, `columns` columns and `payload_len` bytes of columns. The count saturates at `usize::MAX`, a
/// length over [MAX_BLOCK_LEN] no writer takes.
pub fn chunk_head_len(head: &ChunkHead, columns: usize, payload_len: usize) -> usize {
    let grid = GridBytes::of(head).map_or(usize::MAX, |g| g.len);
    let entries = columns.saturating_mul(COLUMN_ENTRY_LEN);
    let crcs = payload_len
        .saturating_add(PAYLOAD_PADDING)
        .div_ceil(SEGMENT_BYTES)
        .saturating_mul(SEGMENT_CRC_LEN);
    FIXED_END
        .saturating_add(grid)
        .saturating_add(entries)
        .saturating_add(crcs)
}

/// Bytes of a chunk's tile coordinates at most: a u32 per fixed axis.
const TILE_COORDS_LEN_MAX: usize = 4 * GRID_FIXED_AXES_MAX;

/// The grid bytes of a series chunk's head (84..101 + scale + tile coordinates), none in an events chunk.
struct GridBytes {
    buf: [u8; SERIES_GRID_LEN + SCALE_LEN_MAX + TILE_COORDS_LEN_MAX],
    len: usize,
}

impl GridBytes {
    /// The bytes of `head`'s grid; an error when its scale doesn't encode or takes more than [SCALE_LEN_MAX].
    fn of(head: &ChunkHead) -> Result<Self, ChunkError> {
        let mut buf = [0u8; SERIES_GRID_LEN + SCALE_LEN_MAX + TILE_COORDS_LEN_MAX];
        let Some(g) = head.grid else {
            return Ok(Self { buf, len: 0 });
        };
        let mut scale = [0u8; SCALE_LEN_MAX];
        let scale_len = scale_bytes(&g.scale, &mut scale).map_err(ChunkError::Scale)?;
        let s = u8::try_from(scale_len)
            .ok()
            .filter(|s| *s != 0)
            .ok_or(ChunkError::Invalid("scale bytes out of range"))?;
        let fields = g
            .grid_index
            .to_le_bytes()
            .into_iter()
            .chain(g.grid_tick.to_le_bytes())
            .chain([s])
            .chain(scale.into_iter().take(scale_len))
            .chain(head.tile.as_slice().iter().flat_map(|c| c.to_le_bytes()));
        for (d, b) in buf.iter_mut().zip(fields) {
            *d = b;
        }
        Ok(Self {
            buf,
            len: SERIES_GRID_LEN
                .saturating_add(scale_len)
                .saturating_add(tile_coords_len(head.tile.len())),
        })
    }

    fn bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }
}

/// Zero bytes to put between the head and the payload of a block written at `offset` in a file.
///
/// They make the payload land on a multiple of `alignment` (0: no alignment, no padding). None when `alignment` is
/// neither 0 nor a power of two.
pub const fn chunk_padding(offset: u64, head_len: usize, alignment: u32) -> Option<usize> {
    if alignment == 0 {
        return Some(0);
    }
    if !alignment.is_power_of_two() {
        return None;
    }
    let Some(mask) = alignment.checked_sub(1) else {
        return None;
    };
    // only the low bits count: the end modulo 2^64 has them
    Some(to_boundary(offset.wrapping_add(len_u64(head_len)), mask))
}

/// Bytes from `end` to the next multiple of the power of two `mask + 1`; at most `mask`.
pub(crate) const fn to_boundary(end: u64, mask: u32) -> usize {
    // the distance to the next multiple is the low bits of -end
    let [b0, b1, b2, b3, ..] = end.wrapping_neg().to_le_bytes();
    u32_len(u32::from_le_bytes([b0, b1, b2, b3]) & mask)
}

/// Bytes of a whole chunk block with `head`: head, `padding`, payload and CRC; saturating at `usize::MAX`, a length
/// over [MAX_BLOCK_LEN] no writer takes.
pub fn chunk_len(head: &ChunkHead, columns: &[ColumnSpec], padding: usize) -> usize {
    GridBytes::of(head)
        .ok()
        .and_then(|g| Parts::new(g.len, columns, padding))
        .and_then(|p| p.len())
        .unwrap_or(usize::MAX)
}

/// Bytes of the parts of a chunk block to write.
struct Parts {
    grid: usize,
    entries: usize,
    seg_crcs: usize,
    padding: usize,
    payload: usize,
}

impl Parts {
    /// None when a length overflows a usize.
    fn new(grid: usize, columns: &[ColumnSpec], padding: usize) -> Option<Self> {
        let columns_len = columns
            .iter()
            .try_fold(0usize, |n, c| n.checked_add(c.len))?;
        let payload = columns_len.checked_add(PAYLOAD_PADDING)?;
        Some(Self {
            grid,
            entries: columns.len().checked_mul(COLUMN_ENTRY_LEN)?,
            seg_crcs: payload
                .div_ceil(SEGMENT_BYTES)
                .checked_mul(SEGMENT_CRC_LEN)?,
            padding,
            payload,
        })
    }

    fn len(&self) -> Option<usize> {
        FIXED_END
            .checked_add(self.grid)?
            .checked_add(self.entries)?
            .checked_add(self.seg_crcs)?
            .checked_add(self.padding)?
            .checked_add(self.payload)?
            .checked_add(CRC_LEN)
    }
}

/// Bytes of the header before the head CRC (0..28).
const PRE_CRC_LEN: usize = 28;

/// Writes one chunk block into a buffer: the head at [Self::new], the columns in place ([Self::column_mut]), the
/// CRCs at [Self::finish].
pub struct ChunkWriter<'b> {
    len: usize,
    /// Header bytes 0..28
    pre: &'b mut [u8; PRE_CRC_LEN],
    head_crc: &'b mut [u8; 4],
    fixed: &'b mut [u8; CHUNK_FIXED_LEN],
    /// The grid of a series chunk, empty in an events chunk
    grid: &'b mut [u8],
    entries: &'b mut [u8],
    seg_crcs: &'b mut [u8],
    /// Zeros up to the payload offset
    padding: &'b mut [u8],
    /// The columns, then [PAYLOAD_PADDING] zeros
    payload: &'b mut [u8],
    crc: &'b mut [u8; CRC_LEN],
    /// Column offsets in the payload, and the payload's length without padding at the end
    offsets: [u32; MAX_COLUMNS + 1],
    columns: usize,
}

impl<'b> ChunkWriter<'b> {
    /// Write the head of a chunk with `columns` (their bytes zero until filled in) and `padding` zero bytes before
    /// the payload ([chunk_padding]).
    pub fn new(
        buf: &'b mut [u8],
        id: crate::BlockId,
        head: &ChunkHead,
        columns: &[ColumnSpec],
        padding: usize,
    ) -> Result<Self, ChunkError> {
        Self::make(buf, id, head, columns, padding, true)
    }

    /// [Self::new]; the column bytes are left as they are in `buf` unless `zero_columns`, for a caller that fills
    /// every column, so the payload is written once. The rest of the block is written or zeroed either way.
    fn make(
        buf: &'b mut [u8],
        id: crate::BlockId,
        head: &ChunkHead,
        columns: &[ColumnSpec],
        padding: usize,
        zero_columns: bool,
    ) -> Result<Self, ChunkError> {
        check_head(head, columns.iter().map(|c| c.kind))?;
        let column_count = u8::try_from(columns.len())
            .ok()
            .ok_or(ChunkError::Invalid("more than 255 columns"))?;
        let grid_bytes = GridBytes::of(head)?;
        let len = chunk_len(head, columns, padding);
        if len > MAX_BLOCK_LEN {
            return Err(ChunkError::TooLong { len });
        }
        let parts =
            Parts::new(grid_bytes.len, columns, padding).ok_or(ChunkError::TooLong { len })?;
        let short = ChunkError::Full { needed: len };
        let buf = buf.get_mut(..len).ok_or(short)?;
        let (pre, rest) = buf.split_first_chunk_mut::<PRE_CRC_LEN>().ok_or(short)?;
        let (head_crc, rest) = rest.split_first_chunk_mut::<4>().ok_or(short)?;
        let (fixed, rest) = rest
            .split_first_chunk_mut::<CHUNK_FIXED_LEN>()
            .ok_or(short)?;
        let (grid, rest) = rest.split_at_mut_checked(parts.grid).ok_or(short)?;
        grid.copy_from_slice(grid_bytes.bytes());
        let (entries, rest) = rest.split_at_mut_checked(parts.entries).ok_or(short)?;
        let (seg_crcs, rest) = rest.split_at_mut_checked(parts.seg_crcs).ok_or(short)?;
        let (gap, rest) = rest.split_at_mut_checked(parts.padding).ok_or(short)?;
        let (payload, crc) = rest.split_last_chunk_mut::<CRC_LEN>().ok_or(short)?;
        // every other byte of the head is written below or in finish; the gap and the payload's padding are zeros
        gap.fill(0);
        if zero_columns {
            payload.fill(0);
        } else if let Some(tail) = payload.last_chunk_mut::<PAYLOAD_PADDING>() {
            tail.fill(0);
        }
        // the payload starts after the head and the padding, within a block of at most MAX_BLOCK_LEN
        let payload_at = len
            .checked_sub(payload.len())
            .and_then(|n| n.checked_sub(CRC_LEN))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(ChunkError::TooLong { len })?;

        let mut header = [0u8; HEADER_LEN];
        crate::put_header(&mut header, head.kind, len, head.count, id)
            .ok_or(ChunkError::TooLong { len })?;
        let [header_pre @ .., _, _, _, _] = header;
        *pre = header_pre;
        let fields = payload_at
            .to_le_bytes()
            .into_iter()
            .chain(head.track_number.get().to_le_bytes())
            .chain(head.track_id.get().to_le_bytes())
            .chain(head.first_item.get().to_le_bytes())
            .chain(head.first_tick.to_le_bytes())
            .chain(head.last_tick.to_le_bytes())
            .chain(head.latest_end.to_le_bytes())
            .chain([
                SEGMENT_SHIFT,
                column_count,
                mode_byte(head)?,
                end_byte(head),
            ]);
        for (d, s) in fixed.iter_mut().zip(fields) {
            *d = s;
        }

        let mut offsets = [0u32; MAX_COLUMNS + 1];
        let mut off = 0u32;
        let (entry_slots, _) = entries.as_chunks_mut::<COLUMN_ENTRY_LEN>();
        for ((slot, entry), c) in offsets.iter_mut().zip(entry_slots).zip(columns) {
            *slot = off;
            let [p0, p1] = c.param.to_le_bytes();
            let [o0, o1, o2, o3] = off.to_le_bytes();
            *entry = [c.kind.to_u8(), c.codec.0, p0, p1, o0, o1, o2, o3];
            // the columns sum to less than the block's length, at most MAX_BLOCK_LEN
            off = u32::try_from(c.len)
                .ok()
                .and_then(|n| off.checked_add(n))
                .ok_or(ChunkError::TooLong { len })?;
        }
        *offsets
            .get_mut(columns.len())
            .ok_or(ChunkError::Invalid("more than 255 columns"))? = off;
        Ok(ChunkWriter {
            len,
            pre,
            head_crc,
            fixed,
            grid,
            entries,
            seg_crcs,
            padding: gap,
            payload,
            crc,
            offsets,
            columns: columns.len(),
        })
    }

    /// The bytes of column `i`, to fill in; None when there is no column `i`.
    pub fn column_mut(&mut self, i: usize) -> Option<&mut [u8]> {
        if i >= self.columns {
            return None;
        }
        let from = u32_len(*self.offsets.get(i)?);
        let to = u32_len(*self.offsets.get(i.checked_add(1)?)?);
        self.payload.get_mut(from..to)
    }

    /// Write the head CRC and a block CRC over the head alone, the segment CRCs left zero, the header marked
    /// [FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED): a chunk of a piece of
    /// [Integrity::None](crate::Integrity::None). Returns the block's length.
    pub fn finish_unchecked(self) -> usize {
        if let Some(f) = self.pre.get_mut(6..).and_then(|r| r.first_chunk_mut::<2>()) {
            *f = (u16::from_le_bytes(*f) | crate::FLAG_PAYLOAD_UNCHECKED).to_le_bytes();
        }
        self.seg_crcs.fill(0);
        let after_crc = [
            self.fixed.as_slice(),
            self.grid,
            self.entries,
            self.seg_crcs,
            self.padding,
        ];
        let mut d = CRC.digest();
        d.update(self.pre);
        after_crc.iter().for_each(|p| d.update(p));
        *self.head_crc = d.finalize().to_le_bytes();
        let mut d = CRC.digest();
        d.update(self.pre);
        d.update(self.head_crc);
        after_crc.iter().for_each(|p| d.update(p));
        *self.crc = d.finalize().to_le_bytes();
        self.len
    }

    /// Write the segment CRCs, the head CRC and the block CRC; returns the block's length.
    pub fn finish(self) -> usize {
        let mut payload_crc: Option<u32> = None;
        let (slots, _) = self.seg_crcs.as_chunks_mut::<SEGMENT_CRC_LEN>();
        for (segment, slot) in self.payload.chunks(SEGMENT_BYTES).zip(slots) {
            let c = CRC.checksum(segment);
            payload_crc = Some(match payload_crc {
                None => c,
                Some(p) => crc32c_combine(p, c, len_u64(segment.len())),
            });
            *slot = c.to_le_bytes();
        }
        let after_crc = [
            self.fixed.as_slice(),
            self.grid,
            self.entries,
            self.seg_crcs,
            self.padding,
        ];
        let mut d = CRC.digest();
        d.update(self.pre);
        after_crc.iter().for_each(|p| d.update(p));
        *self.head_crc = d.finalize().to_le_bytes();
        let mut d = CRC.digest();
        d.update(self.pre);
        d.update(self.head_crc);
        after_crc.iter().for_each(|p| d.update(p));
        let head_all = d.finalize();
        let block_crc = match payload_crc {
            Some(p) => crc32c_combine(head_all, p, len_u64(self.payload.len())),
            None => head_all,
        };
        *self.crc = block_crc.to_le_bytes();
        self.len
    }
}

/// Rewrite the head CRC and the block CRC of a good chunk block whose header changed (its seq), from the segment
/// CRCs it holds: no pass over the payload. A block marked [FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED)
/// gets a block CRC over its head alone. An error says why the block's head is out of range (its head CRC is not
/// checked: the header changed); nothing is written then.
pub(crate) fn restamp(block: &mut [u8]) -> Result<(), ChunkHeadError> {
    let payload_at = payload_offset(block, block.len())?;
    let l = Layout::of(
        block.get(..payload_at).ok_or(ChunkHeadError::BadHead)?,
        block.len(),
    )?;
    let payload = block
        .get(payload_at..l.payload_end)
        .ok_or(ChunkHeadError::BadHead)?;
    let seg_crcs = block
        .get(l.table_end..l.seg_crcs_end)
        .ok_or(ChunkHeadError::BadHead)?;
    let unchecked = le_u16(block, 6).is_some_and(|f| f & crate::FLAG_PAYLOAD_UNCHECKED != 0);
    let mut payload_crc: Option<u32> = None;
    for (segment, c) in payload
        .chunks(l.seg_len.get())
        .zip(seg_crcs.as_chunks::<SEGMENT_CRC_LEN>().0)
        .filter(|_| !unchecked)
    {
        let c = u32::from_le_bytes(*c);
        payload_crc = Some(match payload_crc {
            None => c,
            Some(p) => crc32c_combine(p, c, len_u64(segment.len())),
        });
    }
    let payload_len = len_u64(payload.len());
    let h = head_crc(block, payload_at).ok_or(ChunkHeadError::BadHead)?;
    let (pre, rest) = block
        .split_first_chunk_mut::<PRE_CRC_LEN>()
        .ok_or(ChunkHeadError::BadHead)?;
    let (head_crc_slot, rest) = rest
        .split_first_chunk_mut::<4>()
        .ok_or(ChunkHeadError::BadHead)?;
    *head_crc_slot = h.to_le_bytes();
    let head_rest = rest
        .get(..payload_at.saturating_sub(PRE_CRC_LEN + 4))
        .ok_or(ChunkHeadError::BadHead)?;
    let mut d = CRC.digest();
    d.update(pre);
    d.update(head_crc_slot);
    d.update(head_rest);
    let head_all = d.finalize();
    let crc = match payload_crc {
        Some(p) => crc32c_combine(head_all, p, payload_len),
        None => head_all,
    };
    let (_, crc_slot) = block
        .split_last_chunk_mut::<CRC_LEN>()
        .ok_or(ChunkHeadError::BadHead)?;
    *crc_slot = crc.to_le_bytes();
    Ok(())
}

/// Fill the segment CRCs and the block CRC of the chunk block `block` marked
/// [FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED), clear the mark and rewrite its head CRC: the block a
/// writer of [Integrity::Full](crate::Integrity::Full) writes from the same chunk. An error says why the block's head
/// is out of range; nothing is written then.
#[cfg(feature = "alloc")]
pub(crate) fn fill_payload_crcs(block: &mut [u8]) -> Result<(), ChunkHeadError> {
    let payload_at = payload_offset(block, block.len())?;
    let l = Layout::of(
        block.get(..payload_at).ok_or(ChunkHeadError::BadHead)?,
        block.len(),
    )?;
    let flags = le_u16(block, 6).ok_or(ChunkHeadError::BadHead)? & !crate::FLAG_PAYLOAD_UNCHECKED;
    block
        .get_mut(6..8)
        .ok_or(ChunkHeadError::BadHead)?
        .copy_from_slice(&flags.to_le_bytes());
    let (head, rest) = block
        .split_at_mut_checked(payload_at)
        .ok_or(ChunkHeadError::BadHead)?;
    let payload = rest
        .get(..l.payload_end.saturating_sub(payload_at))
        .ok_or(ChunkHeadError::BadHead)?;
    let slots = head
        .get_mut(l.table_end..l.seg_crcs_end)
        .ok_or(ChunkHeadError::BadHead)?;
    for (segment, slot) in payload
        .chunks(l.seg_len.get())
        .zip(slots.as_chunks_mut::<SEGMENT_CRC_LEN>().0)
    {
        *slot = CRC.checksum(segment).to_le_bytes();
    }
    restamp(block)
}

/// Write a whole chunk block with `columns` copied in; returns its length.
pub fn write_chunk(
    buf: &mut [u8],
    id: crate::BlockId,
    head: &ChunkHead,
    columns: &[(ColumnSpec, &[u8])],
    padding: usize,
) -> Result<usize, ChunkError> {
    encode_chunk(buf, id, head, columns, padding, true)
}

/// [write_chunk], with its payload CRCs (`payload_crcs`) or without them ([ChunkWriter::finish_unchecked]).
pub(crate) fn encode_chunk(
    buf: &mut [u8],
    id: crate::BlockId,
    head: &ChunkHead,
    columns: &[(ColumnSpec, &[u8])],
    padding: usize,
    payload_crcs: bool,
) -> Result<usize, ChunkError> {
    let mut specs = [ColumnSpec {
        kind: ColumnKind::Other(0),
        codec: Codec::RAW,
        param: 0,
        len: 0,
    }; MAX_COLUMNS];
    let specs = specs
        .get_mut(..columns.len())
        .ok_or(ChunkError::Invalid("more than 255 columns"))?;
    for (slot, (spec, bytes)) in specs.iter_mut().zip(columns) {
        if spec.len != bytes.len() {
            return Err(ChunkError::Invalid("column length differs from its bytes"));
        }
        if head.kind == BlockKind::Series
            && spec.kind == ColumnKind::Flags
            && (spec.codec != Codec::FLAG_RUNS || crate::FlagRuns::of(bytes, head.count).is_err())
        {
            return Err(ChunkError::Invalid(
                "a series chunk's flags not as flag runs in the writer's form",
            ));
        }
        *slot = *spec;
    }
    // every column is copied in below: its bytes need no zeros first
    let mut w = ChunkWriter::make(buf, id, head, specs, padding, false)?;
    for (i, (_, bytes)) in columns.iter().enumerate() {
        w.column_mut(i)
            .ok_or(ChunkError::Invalid("a column the writer was not made with"))?
            .copy_from_slice(bytes);
    }
    Ok(if payload_crcs {
        w.finish()
    } else {
        w.finish_unchecked()
    })
}

fn check_head(head: &ChunkHead, kinds: impl Iterator<Item = ColumnKind>) -> Result<(), ChunkError> {
    if !matches!(head.kind, BlockKind::Events | BlockKind::Series) {
        return Err(ChunkError::Invalid("not a chunk block kind"));
    }
    if head.count == 0 {
        return Err(ChunkError::Invalid("no items"));
    }
    if head.first_item.checked_add(u64::from(head.count)).is_none() {
        return Err(ChunkError::Invalid("item numbers past u64"));
    }
    if head.last_tick < head.first_tick {
        return Err(ChunkError::Invalid("last tick before the first"));
    }
    if head.latest_end < head.last_tick {
        return Err(ChunkError::Invalid("latest end before the last tick"));
    }
    if (head.kind == BlockKind::Series || head.end_mode == EndMode::Absent)
        && head.latest_end != head.last_tick
    {
        return Err(ChunkError::Invalid(
            "latest end not the last tick in a chunk of instants or samples",
        ));
    }
    if head.kind == BlockKind::Series && (head.time_mode != 0 || head.end_mode != EndMode::Absent) {
        return Err(ChunkError::Invalid(
            "a series chunk has no time or end mode",
        ));
    }
    if head.level > GRID_OVERVIEW_LEVELS_MAX {
        return Err(ChunkError::Invalid(
            "an overview level over GRID_OVERVIEW_LEVELS_MAX",
        ));
    }
    if head.level != 0 && (head.kind != BlockKind::Series || head.tile.is_empty()) {
        return Err(ChunkError::Invalid(
            "an overview level on a chunk of no fixed axes",
        ));
    }
    match (head.kind, head.grid) {
        (BlockKind::Series, None) => {
            return Err(ChunkError::Invalid("a series chunk without a grid"));
        }
        (BlockKind::Series, Some(g)) if g.grid_index > head.first_item.get() => {
            return Err(ChunkError::Invalid(
                "a grid that starts after the first sample",
            ));
        }
        (BlockKind::Events, Some(_)) => {
            return Err(ChunkError::Invalid("an events chunk with a grid"));
        }
        (BlockKind::Events, None) if !head.tile.is_empty() => {
            return Err(ChunkError::Invalid("an events chunk with tile coordinates"));
        }
        _ => {}
    }
    let mut any = false;
    // an overview holds each statistic once and no samples; the cells as written hold no statistic
    let mut stats = [0u8; 3];
    for k in kinds {
        if !k.fits(head.kind) {
            return Err(ChunkError::Invalid("column of another chunk kind"));
        }
        let stat = match k {
            ColumnKind::OverviewMin => stats.get_mut(0),
            ColumnKind::OverviewMax => stats.get_mut(1),
            ColumnKind::OverviewMean => stats.get_mut(2),
            ColumnKind::Samples if head.level != 0 => {
                return Err(ChunkError::Invalid(
                    "an overview chunk with a samples column",
                ));
            }
            _ => None,
        };
        if let Some(n) = stat {
            *n = n.saturating_add(1);
        }
        any = true;
    }
    if !any {
        return Err(ChunkError::Invalid("no columns"));
    }
    let want = u8::from(head.level != 0);
    if stats.iter().any(|n| *n != want) {
        return Err(ChunkError::Invalid(
            "an overview chunk without one min, max and mean column each, or another chunk with one",
        ));
    }
    Ok(())
}

/// CRC-32C over bytes 0..28 and 32..`payload_at` of a chunk block; None when the block is shorter.
fn head_crc(block: &[u8], payload_at: usize) -> Option<u32> {
    let mut d = CRC.digest();
    d.update(block.get(..PRE_CRC_LEN)?);
    d.update(block.get(PRE_CRC_LEN + 4..payload_at)?);
    Some(d.finalize())
}

// crc32c_combine lives in crc32c: the hardware kernels' three-way interleave uses it too.
pub use crate::crc32c::crc32c_combine;

/// Why a chunk block's head can't be read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChunkHeadError {
    /// The head CRC doesn't match: nothing in the block can be trusted, its length neither.
    BadHeadCrc,
    /// The head is out of range: payload offset, segment shift, columns, count, ticks or column offsets.
    BadHead,
    /// A layout value of the head (segment shift, column count, column offset) is over its bound.
    Bound(BoundError),
}

impl core::fmt::Display for ChunkHeadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadHeadCrc => write!(f, "the head CRC doesn't match"),
            Self::BadHead => write!(f, "the head is out of range"),
            Self::Bound(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for ChunkHeadError {}

/// Where the parts of a chunk block lie, from its head's ranges alone (no CRC).
#[derive(Copy, Clone, Debug)]
struct Layout {
    seg_len: NonZeroUsize,
    columns: usize,
    /// Start of the column entries: after the fixed head and, in a series chunk, its grid
    table_start: usize,
    /// End of the column entries
    table_end: usize,
    /// End of the segment CRCs, at most the payload offset
    seg_crcs_end: usize,
    /// End of the payload: the block's length less its CRC
    payload_end: usize,
}

/// The payload offset of a chunk block of `block_len` bytes (bytes 32..36 of `header`, its first bytes): after the
/// fixed head, with room for the padding and the CRC after it. Read before the head CRC can be checked, so out of
/// range is [ChunkHeadError::BadHeadCrc].
fn payload_offset(header: &[u8], block_len: usize) -> Result<usize, ChunkHeadError> {
    let Some(max) = block_len.checked_sub(PAYLOAD_PADDING + CRC_LEN) else {
        return Err(ChunkHeadError::BadHeadCrc);
    };
    let Some(at) = file_u32(header, 32) else {
        return Err(ChunkHeadError::BadHeadCrc);
    };
    match at.within(max) {
        Ok(at) if at >= FIXED_END => Ok(at),
        _ => Err(ChunkHeadError::BadHeadCrc),
    }
}

impl Layout {
    /// The layout of a chunk block of `block_len` bytes whose head is `head` (its bytes up to the payload offset,
    /// [payload_offset]).
    fn of(head: &[u8], block_len: usize) -> Result<Self, ChunkHeadError> {
        let payload_at = head.len();
        let payload_end = block_len
            .checked_sub(CRC_LEN)
            .ok_or(ChunkHeadError::BadHead)?;
        let payload_len = payload_end
            .checked_sub(payload_at)
            .ok_or(ChunkHeadError::BadHead)?;
        let shift = file_u8(head, 80)
            .ok_or(ChunkHeadError::BadHead)?
            .bounded(SEGMENT_SHIFT_MAX)
            .map_err(ChunkHeadError::Bound)?;
        if shift < SEGMENT_SHIFT_MIN {
            return Err(ChunkHeadError::BadHead);
        }
        let columns = file_u8(head, 81)
            .ok_or(ChunkHeadError::BadHead)?
            .at_most(MAX_COLUMNS)
            .map_err(ChunkHeadError::Bound)?;
        if columns == 0 {
            return Err(ChunkHeadError::BadHead);
        }
        let table_start = match BlockKind::from_u8(le_u8(head, 5).ok_or(ChunkHeadError::BadHead)?) {
            BlockKind::Series => {
                let scale_len = file_u8(head, GRID_SCALE_LEN_AT)
                    .ok_or(ChunkHeadError::BadHead)?
                    .at_most(SCALE_LEN_MAX)
                    .map_err(ChunkHeadError::Bound)?;
                if scale_len == 0 {
                    return Err(ChunkHeadError::BadHead);
                }
                let axes = file_u8(head, AXES_AT)
                    .ok_or(ChunkHeadError::BadHead)?
                    .at_most(GRID_FIXED_AXES_MAX)
                    .map_err(ChunkHeadError::Bound)?;
                FIXED_END
                    .checked_add(SERIES_GRID_LEN)
                    .and_then(|n| n.checked_add(scale_len))
                    .and_then(|n| n.checked_add(tile_coords_len(axes)))
                    .ok_or(ChunkHeadError::BadHead)?
            }
            _ => FIXED_END,
        };
        let seg_len = 1usize
            .checked_shl(u32::from(shift))
            .and_then(NonZeroUsize::new)
            .ok_or(ChunkHeadError::BadHead)?;
        // at most MAX_SEGMENTS: the payload is under MAX_BLOCK_LEN, a segment at least 2^SEGMENT_SHIFT_MIN
        let segments = payload_len.div_ceil(seg_len.get());
        let table_end = columns
            .checked_mul(COLUMN_ENTRY_LEN)
            .and_then(|n| n.checked_add(table_start))
            .ok_or(ChunkHeadError::BadHead)?;
        let seg_crcs_end = segments
            .checked_mul(SEGMENT_CRC_LEN)
            .and_then(|n| n.checked_add(table_end))
            .filter(|end| *end <= payload_at)
            .ok_or(ChunkHeadError::BadHead)?;
        Ok(Self {
            seg_len,
            columns,
            table_start,
            table_end,
            seg_crcs_end,
            payload_end,
        })
    }
}

/// The fixed part of the head of `block` of `kind`, with its grid in a series chunk (its scale ending at
/// `table_start`); None when the block is shorter, the end mode unknown or the scale not one.
fn read_fixed(block: &[u8], kind: BlockKind, table_start: usize) -> Option<ChunkHead> {
    let mode = le_u8(block, AXES_AT)?;
    let end = le_u8(block, END_AT)?;
    let (grid, tile, time_mode) = match kind {
        BlockKind::Series => {
            // the layout bounded the axes and put the coordinates last before the column table
            let coords_at = table_start.checked_sub(tile_coords_len(usize::from(mode)))?;
            let mut coords = [0u32; GRID_FIXED_AXES_MAX];
            let bytes = block.get(coords_at..table_start)?.as_chunks::<4>().0;
            for (d, c) in coords.iter_mut().zip(bytes) {
                *d = u32::from_le_bytes(*c);
            }
            let grid = SeriesGrid {
                grid_index: le_u64(block, FIXED_END)?,
                grid_tick: le_i64(block, GRID_TICK_AT)?,
                scale: read_scale(block.get(GRID_SCALE_AT..coords_at)?).ok()?,
            };
            let tile = TileCoords::new(coords.get(..usize::from(mode))?)?;
            (Some(grid), tile, 0)
        }
        _ => (None, TileCoords::NONE, mode),
    };
    Some(ChunkHead {
        kind,
        track_id: TrackId::new(le_u64(block, 40)?),
        track_number: TrackNum::try_new(le_u32(block, 36)?)?,
        first_item: ItemNum::new(le_u64(block, 48)?),
        count: le_u32(block, 12)?,
        first_tick: le_i64(block, 56)?,
        last_tick: le_i64(block, 64)?,
        latest_end: le_i64(block, 72)?,
        time_mode,
        // a series chunk's byte 83 is its overview level, checked with the head
        end_mode: match kind {
            BlockKind::Series => EndMode::Absent,
            _ => EndMode::from_u8(end)?,
        },
        level: if kind == BlockKind::Series { end } else { 0 },
        grid,
        tile,
    })
}

/// The head of a chunk block read from its bytes up to the payload offset, checked: what [Chunk::read_head] and
/// [ChunkHead::read_head] share.
struct HeadBytes<'b> {
    head: ChunkHead,
    layout: Layout,
    entries: &'b [[u8; COLUMN_ENTRY_LEN]],
    seg_crcs: &'b [[u8; SEGMENT_CRC_LEN]],
}

impl<'b> HeadBytes<'b> {
    /// Read `bytes`, a chunk block's bytes 0..payload offset, in a block of `block_len` bytes: head CRC, ranges,
    /// column entries and their offsets within the payload, zeros between the head and the payload.
    fn read(bytes: &'b [u8], block_len: usize) -> Result<Self, ChunkHeadError> {
        let kind = BlockKind::from_u8(le_u8(bytes, 5).ok_or(ChunkHeadError::BadHeadCrc)?);
        if head_crc(bytes, bytes.len()) != le_u32(bytes, 28) {
            return Err(ChunkHeadError::BadHeadCrc);
        }
        let l = Layout::of(bytes, block_len)?;
        let head = read_fixed(bytes, kind, l.table_start).ok_or(ChunkHeadError::BadHead)?;
        let entries = bytes
            .get(l.table_start..l.table_end)
            .ok_or(ChunkHeadError::BadHead)?
            .as_chunks::<COLUMN_ENTRY_LEN>()
            .0;
        if entries.len() != l.columns
            || check_head(&head, entries.iter().map(|[k, ..]| ColumnKind::from_u8(*k))).is_err()
        {
            return Err(ChunkHeadError::BadHead);
        }
        // the payload lies between the head and the block CRC, its padding last
        let columns_len = l
            .payload_end
            .checked_sub(bytes.len())
            .and_then(|n| n.checked_sub(PAYLOAD_PADDING))
            .ok_or(ChunkHeadError::BadHead)?;
        let mut prev = 0usize;
        for (i, e) in entries.iter().enumerate() {
            let off = column_offset(e)
                .within(columns_len)
                .map_err(ChunkHeadError::Bound)?;
            if (i == 0 && off != 0) || off < prev {
                return Err(ChunkHeadError::BadHead);
            }
            prev = off;
        }
        let (seg_crcs, _) = bytes
            .get(l.table_end..l.seg_crcs_end)
            .ok_or(ChunkHeadError::BadHead)?
            .as_chunks::<SEGMENT_CRC_LEN>();
        // padding between the head and the payload is zeros
        let gap = bytes.get(l.seg_crcs_end..).ok_or(ChunkHeadError::BadHead)?;
        if gap.iter().any(|b| *b != 0) {
            return Err(ChunkHeadError::BadHead);
        }
        Ok(HeadBytes {
            head,
            layout: l,
            entries,
            seg_crcs,
        })
    }
}

/// The chunk of a chunk block, read in place.
#[derive(Copy, Clone, Debug)]
pub struct Chunk<'b> {
    head: ChunkHead,
    /// Bytes 0..payload offset
    head_bytes: &'b [u8],
    /// The column entries, [COLUMN_ENTRY_LEN] bytes each, offsets checked ascending within the columns
    entries: &'b [[u8; COLUMN_ENTRY_LEN]],
    /// One CRC per segment of the payload
    seg_crcs: &'b [[u8; SEGMENT_CRC_LEN]],
    /// The columns, then [PAYLOAD_PADDING] bytes
    payload: &'b [u8],
    block_crc: u32,
    seg_len: NonZeroUsize,
    /// End of the column entries in the head
    table_end: usize,
    /// The block is marked [FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED): its segment CRCs are zero and its
    /// block CRC covers the head only
    unchecked: bool,
}

impl<'b> Chunk<'b> {
    /// Read and check the head of the chunk block `block` (exactly its bytes, length checked by the caller): head
    /// CRC, ranges. The payload is not checked here.
    pub(crate) fn read_head(block: &'b [u8]) -> Result<Self, ChunkHeadError> {
        let payload_at = payload_offset(block, block.len())?;
        let head_bytes = block.get(..payload_at).ok_or(ChunkHeadError::BadHeadCrc)?;
        let h = HeadBytes::read(head_bytes, block.len())?;
        Ok(Chunk {
            head: h.head,
            head_bytes,
            entries: h.entries,
            seg_crcs: h.seg_crcs,
            payload: block
                .get(payload_at..h.layout.payload_end)
                .ok_or(ChunkHeadError::BadHead)?,
            block_crc: le_u32(block, h.layout.payload_end).ok_or(ChunkHeadError::BadHead)?,
            seg_len: h.layout.seg_len,
            table_end: h.layout.table_end,
            unchecked: le_u16(block, 6).is_some_and(|f| f & crate::FLAG_PAYLOAD_UNCHECKED != 0),
        })
    }

    /// Whether the chunk's writer computed no CRCs over its payload
    /// ([FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED), [Integrity::None](crate::Integrity::None)): its
    /// payload is taken as it is, and no segment reads as bad.
    pub fn is_unchecked(&self) -> bool {
        self.unchecked
    }

    /// The bytes of the head that say what the chunk holds, not where it lies, for its track hash
    /// ([TrackHash](crate::piece::TrackHash)): bytes 48..80 (first item, ticks, latest end) and 81 to the end of
    /// the column entries (columns, time and end mode, a series grid and scale, the entries).
    pub(crate) fn descriptor(&self) -> (&'b [u8], &'b [u8]) {
        let times = self.head_bytes.get(48..80).unwrap_or_default();
        let columns = self.head_bytes.get(81..self.table_end).unwrap_or_default();
        (times, columns)
    }

    /// The fixed part of the head.
    pub fn head(&self) -> ChunkHead {
        self.head
    }

    /// Whether the chunk is an events chunk whose track's monotone relations start again
    /// ([FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK)).
    pub fn relation_break(&self) -> bool {
        self.head.kind == BlockKind::Events
            && le_u16(self.head_bytes, 6).is_some_and(|f| f & crate::FLAG_RELATION_BREAK != 0)
    }

    /// Where the payload starts in the block.
    pub fn payload_offset(&self) -> usize {
        self.head_bytes.len()
    }

    /// The payload: the columns, then [PAYLOAD_PADDING] bytes (zeros in an intact block).
    pub fn payload(&self) -> &'b [u8] {
        self.payload
    }

    pub fn column_count(&self) -> usize {
        self.entries.len()
    }

    /// Column `i`, None past the last.
    pub fn column(&self, i: usize) -> Option<Column<'b>> {
        let entry = self.entries.get(i)?;
        let columns_len = self.payload.len().checked_sub(PAYLOAD_PADDING)?;
        // offsets were checked ascending and within the columns when the head was read
        let offset = column_offset(entry).within(columns_len).ok()?;
        let end = match self.entries.get(i.checked_add(1)?) {
            Some(next) => column_offset(next).within(columns_len).ok()?,
            None => columns_len,
        };
        let [kind, codec, p0, p1, ..] = *entry;
        Some(Column {
            kind: ColumnKind::from_u8(kind),
            codec: Codec(codec),
            param: u16::from_le_bytes([p0, p1]),
            offset,
            bytes: self.payload.get(offset..end)?,
        })
    }

    pub fn columns(&self) -> impl Iterator<Item = Column<'b>> + '_ {
        (0..self.entries.len()).filter_map(|i| self.column(i))
    }

    /// The first column of `kind` (and `param`, for samples).
    pub fn find(&self, kind: ColumnKind, param: u16) -> Option<Column<'b>> {
        self.columns().find(|c| c.kind == kind && c.param == param)
    }

    /// The flag runs of a series chunk's samples ([flag_runs](crate::flag_runs)): its first [ColumnKind::Flags]
    /// column, [FlagRuns::NONE](crate::FlagRuns::NONE) when it has none. Refused for a column in another codec than
    /// [Codec::FLAG_RUNS] or not in the writer's form.
    pub fn flag_runs(&self) -> Result<crate::FlagRuns<'b>, crate::FlagRunsError> {
        let Some(c) = self.find(ColumnKind::Flags, 0) else {
            return Ok(crate::FlagRuns::NONE);
        };
        if c.codec != Codec::FLAG_RUNS {
            return Err(crate::FlagRunsError::Codec(c.codec));
        }
        crate::FlagRuns::of(c.bytes, self.head.count)
    }

    /// Bytes of a segment, 2^shift.
    pub fn segment_len(&self) -> usize {
        self.seg_len.get()
    }

    pub fn segment_count(&self) -> usize {
        self.seg_crcs.len()
    }

    /// The segments with their CRCs, in order.
    fn segments(&self) -> impl Iterator<Item = (&'b [u8], u32)> + '_ {
        self.payload
            .chunks(self.seg_len.get())
            .zip(self.seg_crcs.iter().map(|c| u32::from_le_bytes(*c)))
    }

    /// Segment `i` of the payload, None past the last.
    fn segment(&self, i: usize) -> Option<&'b [u8]> {
        self.payload.chunks(self.seg_len.get()).nth(i)
    }

    /// Whether segment `i` doesn't match its CRC; false past the last segment.
    fn segment_bad(&self, i: usize) -> bool {
        if self.unchecked {
            return false;
        }
        match (self.segment(i), self.seg_crcs.get(i)) {
            (Some(s), Some(c)) => CRC.checksum(s) != u32::from_le_bytes(*c),
            _ => false,
        }
    }

    /// Check every segment against its CRC and the block CRC against them: true when all match. One pass over the
    /// payload. A chunk marked [FLAG_PAYLOAD_UNCHECKED](crate::FLAG_PAYLOAD_UNCHECKED) has its block CRC checked
    /// over its head alone and its padding read; its segment CRC slots must be zero.
    pub(crate) fn check_payload(&self) -> bool {
        let mut combined = CRC.checksum(self.head_bytes);
        if self.unchecked {
            return combined == self.block_crc
                && self.seg_crcs.iter().all(|c| *c == [0; SEGMENT_CRC_LEN])
                && self
                    .payload
                    .last_chunk::<PAYLOAD_PADDING>()
                    .is_some_and(|p| p.iter().all(|b| *b == 0));
        }
        for (s, crc) in self.segments() {
            let c = CRC.checksum(s);
            if c != crc {
                return false;
            }
            combined = crc32c_combine(combined, c, len_u64(s.len()));
        }
        combined == self.block_crc
            && self
                .payload
                .last_chunk::<PAYLOAD_PADDING>()
                .is_some_and(|p| p.iter().all(|b| *b == 0))
    }

    /// The segments whose bytes don't match their CRC, ascending. Empty for an intact block.
    pub fn bad_segments(&self) -> impl Iterator<Item = usize> + '_ {
        self.segments()
            .enumerate()
            .filter(|(_, (s, c))| !self.unchecked && CRC.checksum(s) != *c)
            .map(|(i, _)| i)
    }

    /// Whether any byte in `range` of the payload lies in a bad segment.
    pub fn range_damaged(&self, range: core::ops::Range<usize>) -> bool {
        if range.is_empty() {
            return false;
        }
        let Some(last_byte) = range.end.checked_sub(1) else {
            return false;
        };
        let first = range.start / self.seg_len;
        let last = last_byte / self.seg_len;
        (first..=last)
            .take_while(|i| *i < self.segment_count())
            .any(|i| self.segment_bad(i))
    }

    /// Whether any byte of `column`, or of the [PAYLOAD_PADDING] bytes a codec may load past its end, is in a bad
    /// segment.
    pub fn column_damaged(&self, column: &Column<'_>) -> bool {
        // past the payload there is nothing to be damaged: the end stops there
        let end = column
            .offset
            .saturating_add(column.bytes.len())
            .saturating_add(PAYLOAD_PADDING)
            .min(self.payload.len());
        self.range_damaged(column.offset..end)
    }
}

/// The offset field of a column entry.
fn column_offset(entry: &[u8; COLUMN_ENTRY_LEN]) -> crate::FromFile<u32> {
    let [.., o0, o1, o2, o3] = *entry;
    crate::FromFile::new(u32::from_le_bytes([o0, o1, o2, o3]))
}

#[cfg(test)]
mod tests;
