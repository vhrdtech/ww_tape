//! The fault at minute 52 found in code: the program of the book's Part IV.
//!
//! An engineer opens the drive's chain ([open]), finds the blocks of `phase_a` around the fault and the mode in force
//! there ([find], [mode]), reads the CAN frames after it ([frames]) and the current before it ([current]). A viewer
//! draws the hour of `phase_a` from its summaries and zooms to the fault ([overview]). The chain's links, each
//! piece's hash and a range of frames in a copy are checked ([check]). The chapters include the marked sections of
//! this file; [Fault::find] runs them all and counts the bytes each step reads ([Counted]).

use core::cell::Cell;
use ww_tape::ItemNum;
use ww_tape::TrackNum;

use shrink_wrap::prelude::*;
use ww_tape::chunk::ColumnKind;
use ww_tape::index::{ENTRY_LEN, IndexEntry};
use ww_tape::payloads::{Payloads, PayloadsError};
use ww_tape::reader::{ByteSource, OutOfRange, ReadError};
use ww_tape::samples::{SampleShape, Samples};
use ww_tape::times::{EventTimes, TimesError};
use ww_tape::{
    BlockKind, Describe, Element, ElementDesc, FileBlocks, Magnitude, OwnerId, Ratio64, Recovery,
    Rounding, SampleError, Slot, TapeReader, TrackHash, TrackKind,
};
use ww_tape_summary::{ChannelStats, SeriesSummarySpec, Summary};

use crate::{EMERGENCY_ID, FAULT_MINUTE, key};

// --8<-- [start:fault_consts_item]
/// Microseconds of the board's clock in a minute.
const US_PER_MINUTE: i64 = 60_000_000;
/// Pixel columns of the viewer's plot of the hour.
pub const COLUMNS: usize = 600;
/// [COLUMNS] as a number of µs to divide.
const COLUMNS_I64: i64 = 600;
/// Microseconds of the current before the fault that [current] reads.
pub const BEFORE_US: i64 = 20_000;
/// Microseconds of frames after the fault that [frames] lists.
pub const AFTER_US: i64 = 5_000;
// --8<-- [end:fault_consts_item]

/// A byte source over `bytes` that counts what a reader reads: each step of [Fault::find] reports its bytes.
pub struct Counted<'a> {
    bytes: &'a [u8],
    read: Cell<u64>,
    reads: Cell<u64>,
}

impl<'a> Counted<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            read: Cell::new(0),
            reads: Cell::new(0),
        }
    }

    /// The bytes and the reads since the last call, and start counting again.
    pub fn take(&self) -> Read {
        Read {
            bytes: self.read.replace(0),
            reads: self.reads.replace(0),
        }
    }
}

impl ByteSource for Counted<'_> {
    type Error = OutOfRange;

    fn size(&self) -> Result<u64, OutOfRange> {
        self.bytes.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OutOfRange> {
        self.read.set(self.read.get() + buf.len() as u64);
        self.reads.set(self.reads.get() + 1);
        self.bytes.read_at(offset, buf)
    }
}

/// What one step read.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Read {
    pub bytes: u64,
    pub reads: u64,
}

/// Why the fault could not be found.
#[derive(Debug)]
#[non_exhaustive]
pub enum FaultError<E> {
    /// Reading the tape.
    Read(ReadError<E>),
    /// The time columns of a chunk of frames.
    Times(TimesError),
    /// The payload columns of a chunk of frames.
    Payloads(PayloadsError),
    /// The samples of a chunk of `phase_a`.
    Samples(SampleError),
    /// Walking a piece's blocks.
    Walk(ww_tape::FileError),
    /// Something the drive's tape holds is not there.
    Missing(&'static str),
}

impl<E: core::fmt::Debug> core::fmt::Display for FaultError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "reading the tape: {e}"),
            Self::Times(e) => write!(f, "the times of a chunk of frames: {e}"),
            Self::Payloads(e) => write!(f, "the payloads of a chunk of frames: {e}"),
            Self::Samples(e) => write!(f, "the samples of phase_a: {e:?}"),
            Self::Walk(e) => write!(f, "walking a piece's blocks: {e:?}"),
            Self::Missing(what) => write!(f, "the tape holds no {what}"),
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for FaultError<E> {}

impl<E> From<ReadError<E>> for FaultError<E> {
    fn from(e: ReadError<E>) -> Self {
        Self::Read(e)
    }
}

// --8<-- [start:fault_us_item]
/// The ticks of the fault on the board's clock, µs since boot.
pub const fn fault_us() -> i64 {
    FAULT_MINUTE as i64 * US_PER_MINUTE
}
// --8<-- [end:fault_us_item]

// --8<-- [start:opened_item]
/// What [open] found: the pieces, and the one that holds the fault.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub pieces: usize,
    pub piece: usize,
}
// --8<-- [end:opened_item]

/// Open the tape in `src` and find the piece whose `phase_a` reaches the fault, from what opening read alone.
pub fn open<S: ByteSource>(src: S) -> Result<(TapeReader<S>, Opened), FaultError<S::Error>> {
    // --8<-- [start:open]
    let reader = TapeReader::open(src)?; // the tails, the closes and the heads, last piece first
    let fault = fault_us(); // 52:00 on the board's µs clock
    // the piece whose close says phase_a reaches the fault: no read beyond what opening read
    let piece = reader
        .pieces()
        .iter()
        .position(|p| {
            p.close.as_ref().is_some_and(|(_, tracks)| {
                tracks.iter().any(|t| {
                    t.track_id == key("phase_a").id()
                        && t.first_tick <= fault
                        && fault <= t.last_tick
                })
            })
        })
        .ok_or(FaultError::Missing("piece holding the fault"))?;
    // --8<-- [end:open]
    let pieces = reader.pieces().len();
    Ok((reader, Opened { pieces, piece }))
}

// --8<-- [start:found_item]
/// What [find] found: the blocks of `phase_a` in the piece and those within a second of the fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// Entries in the track's list: its chunks and summary blocks.
    pub listed: usize,
    /// The chunks of the second before the fault and of the second after it, in file order.
    pub chunks: Vec<IndexEntry>,
}
// --8<-- [end:found_item]

/// Find the chunks of `phase_a` in piece `piece` that hold a second either side of the fault.
pub fn find<S: ByteSource>(
    reader: &TapeReader<S>,
    piece: usize,
) -> Result<Found, FaultError<S::Error>> {
    let fault = fault_us();
    // --8<-- [start:find]
    let (from, to) = (fault - 1_000_000, fault + 1_000_000); // a second either side
    let entries = reader.track_entries(piece, key("phase_a").id())?; // the directory and one track's list
    let chunks: Vec<IndexEntry> = entries
        .iter()
        .filter(|e| e.kind == BlockKind::Series && e.first_tick < to && e.last_tick >= from)
        .copied()
        .collect();
    // --8<-- [end:find]
    Ok(Found {
        listed: entries.len(),
        chunks,
    })
}

// --8<-- [start:in_force_item]
/// The mode in force at a place, as [mode] found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InForce {
    /// The item in force.
    pub item: Vec<u8>,
    /// Bytes between the record and the place.
    pub behind: u64,
    /// The record's time, µs since boot.
    pub at_us: i64,
    /// Nothing newer can hide: no block lost, every stretch covered.
    pub whole: bool,
}
// --8<-- [end:in_force_item]

/// The mode in force where each of `chunks` starts.
pub fn mode<S: ByteSource>(
    reader: &TapeReader<S>,
    piece: usize,
    chunks: &[IndexEntry],
) -> Result<Vec<InForce>, FaultError<S::Error>> {
    let mut out = Vec::new();
    let Some(first) = chunks.first() else {
        return Ok(out);
    };
    let mode = number_of(reader, piece, first.offset, "mode")?;
    for chunk in chunks {
        // --8<-- [start:mode]
        let lookup = reader.state_at(piece, Slot::State { track: mode }, chunk.offset)?;
        let record = lookup.item.as_ref().ok_or(FaultError::Missing("mode"))?;
        // record.item is the mode in force; record.offset is where its block starts
        // --8<-- [end:mode]
        out.push(InForce {
            item: record.item.clone(),
            behind: chunk.offset - record.offset,
            at_us: record.at.map_or(0, |t| t.ticks),
            whole: lookup.is_whole(),
        });
    }
    Ok(out)
}

// --8<-- [start:frame_types_item]
/// A CAN frame as [frames] read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Its item number on `can0`.
    pub item: ItemNum,
    /// Its time, µs since boot.
    pub us: i64,
    /// Its 11-bit identifier.
    pub id: u32,
    /// The bytes after the identifier.
    pub data: Vec<u8>,
}

/// What [frames] read: the chunk that holds the first frame at or after the fault, and the frames of [AFTER_US].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frames {
    /// Frames in the chunk.
    pub in_chunk: usize,
    /// The chunk's block, bytes.
    pub block_len: u32,
    pub frames: Vec<Frame>,
}
// --8<-- [end:frame_types_item]

/// Read the frames of `can0` in piece `piece` from the first at or after the fault, for [AFTER_US].
pub fn frames<S: ByteSource>(
    reader: &TapeReader<S>,
    piece: usize,
) -> Result<Frames, FaultError<S::Error>> {
    let fault = fault_us();
    // --8<-- [start:frames]
    // the first chunk of can0 whose last frame is at or after the fault
    let entry = reader
        .track_entries(piece, key("can0").id())?
        .into_iter()
        .find(|e| e.kind == BlockKind::Events && e.last_tick >= fault)
        .ok_or(FaultError::Missing("frame after the fault"))?;
    let mut buf = Vec::new();
    let block = reader.read_block(piece, &entry, &mut buf)?; // read and checked by its CRC
    let chunk = block
        .chunk()
        .ok_or(FaultError::Missing("chunk of frames"))?;
    let times = EventTimes::read(&chunk).map_err(FaultError::Times)?;
    let payloads = Payloads::read(&chunk)
        .map_err(FaultError::Payloads)?
        .ok_or(FaultError::Missing("payloads"))?;
    let mut frames = Vec::new();
    for j in times.starts().lower_bound(fault)..times.len() {
        let us = times.start(j);
        if us >= fault + AFTER_US {
            break;
        }
        let payload = payloads.get(j).ok_or(FaultError::Missing("payload"))?;
        let (id, data) = payload
            .split_first_chunk::<4>()
            .ok_or(FaultError::Missing("id"))?;
        frames.push(Frame {
            item: chunk.head().first_item.saturating_add(j as u64), // the frame's item number on can0
            us,
            id: u32::from_le_bytes(*id),
            data: data.to_vec(),
        });
    }
    // --8<-- [end:frames]
    Ok(Frames {
        in_chunk: times.len(),
        block_len: entry.len,
        frames,
    })
}

// --8<-- [start:current_type_item]
/// What [current] read: the samples of `phase_a` for [BEFORE_US] up to the fault.
#[derive(Clone, Debug, PartialEq)]
pub struct Current {
    /// The track's ticks per sample.
    pub period: (u64, u64),
    /// The scale of the chunk's numbers.
    pub scale: Magnitude,
    /// The first sample read, its item number.
    pub first: u64,
    /// The samples, stored numbers.
    pub samples: Vec<i16>,
    /// The largest magnitude among them, its item number and its stored number.
    pub peak: (u64, i16),
    /// Samples in the chunk, and the bytes of its block.
    pub in_chunk: u32,
    pub block_len: u32,
}
// --8<-- [end:current_type_item]

// --8<-- [start:series_def_item]
/// The period and the summaries of `phase_a`, found by its key among the definitions in force at `offset` of piece
/// `piece`.
fn series_def<S: ByteSource>(
    reader: &TapeReader<S>,
    piece: usize,
    offset: u64,
) -> Result<(Ratio64, Option<SeriesSummarySpec>), FaultError<S::Error>> {
    let defs = reader.definitions_at(piece, offset)?;
    let number = defs
        .track_number(key("phase_a").id(), [0; 32])
        .ok_or(FaultError::Missing("phase_a"))?;
    let state = defs
        .items
        .get(&Slot::Track { number })
        .ok_or(FaultError::Missing("definition of phase_a"))?;
    let Ok(Describe::Track(def)) = Describe::from_ww_bytes(&state.item) else {
        return Err(FaultError::Missing("track definition that decodes"));
    };
    let TrackKind::Series(series) = def.kind else {
        return Err(FaultError::Missing("series definition"));
    };
    Ok((series.period, series.summaries))
}
// --8<-- [end:series_def_item]

// --8<-- [start:number_of_item]
/// The number of the drive's track `name` among the definitions in force at `offset` of piece `piece`: a reader
/// finds a track by its key.
fn number_of<S: ByteSource>(
    reader: &TapeReader<S>,
    piece: usize,
    offset: u64,
    name: &'static str,
) -> Result<TrackNum, FaultError<S::Error>> {
    reader
        .definitions_at(piece, offset)?
        .track_number(key(name).id(), [0; 32])
        .ok_or(FaultError::Missing(name))
}
// --8<-- [end:number_of_item]

/// Read the samples of `phase_a` in `chunks` for [BEFORE_US] up to the fault.
pub fn current<S: ByteSource>(
    reader: &TapeReader<S>,
    piece: usize,
    chunks: &[IndexEntry],
) -> Result<Current, FaultError<S::Error>> {
    let fault = fault_us();
    let from = fault - BEFORE_US;
    let entry = chunks
        .iter()
        .find(|e| e.first_tick <= from && from <= e.last_tick)
        .ok_or(FaultError::Missing("chunk before the fault"))?;
    let (period, _) = series_def(reader, piece, entry.offset)?;
    // --8<-- [start:current]
    let mut buf = Vec::new();
    let block = reader.read_block(piece, entry, &mut buf)?;
    let chunk = block
        .chunk()
        .ok_or(FaultError::Missing("chunk of samples"))?;
    let head = chunk.head();
    let grid = head.grid.ok_or(FaultError::Missing("grid"))?;
    // sample `i` is at grid_tick + (i - grid_index) · period: the first at or after `from`
    let steps = period
        .recip()
        .apply_rounded(from - grid.grid_tick, Rounding::Ceil);
    let first = grid.grid_index
        + steps
            .and_then(|s| u64::try_from(s).ok())
            .ok_or(FaultError::Missing("sample"))?;
    let at = head
        .first_item
        .offset_to(ItemNum::new(first))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or(FaultError::Missing("sample"))?; // in the chunk
    let n = period
        .recip()
        .apply(BEFORE_US)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or(FaultError::Missing("sample"))?;
    let column = chunk
        .find(ColumnKind::Samples, 0)
        .ok_or(FaultError::Missing("samples"))?;
    let samples = Samples::of(
        &column,
        SampleShape::scalar(ElementDesc::of(Element::I16)),
        head.count as usize,
    )
    .map_err(FaultError::Samples)?;
    let mut ma = vec![0i16; n]; // stored numbers: steps of grid.scale, 10⁻³ A
    samples
        .decode(at, &mut ma) // from the group that holds sample `at`
        .map_err(FaultError::Samples)?;
    // --8<-- [end:current]
    let (k, v) = ma
        .iter()
        .enumerate()
        .max_by_key(|(_, v)| v.unsigned_abs())
        .ok_or(FaultError::Missing("samples"))?;
    Ok(Current {
        period: period.parts(),
        scale: grid.scale,
        first,
        peak: (first + k as u64, *v),
        samples: ma,
        in_chunk: head.count,
        block_len: entry.len,
    })
}

/// One pixel column of the viewer's plot: the statistics of its samples.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Column {
    pub stats: ChannelStats,
    /// Samples the column's nodes cover.
    pub samples: u64,
}

/// What [overview] read: `phase_a`'s hour in [COLUMNS] columns from one level of its summaries, and the zoom into the
/// column of the highest peak.
#[derive(Clone, Debug, PartialEq)]
pub struct Overview {
    /// The level read, and the samples one of its nodes covers.
    pub level: u8,
    pub node_samples: u64,
    /// Summary blocks of that level and their nodes.
    pub blocks: u64,
    pub nodes: u64,
    /// The columns, None where no node fell.
    pub columns: Vec<Option<Column>>,
    /// The column of the highest peak, the minute it starts at, and its peak, stored numbers.
    pub peak_column: usize,
    pub peak: (i64, i64),
    /// The level-0 node of that column with the highest peak: its first sample.
    pub zoom: u64,
    /// The bytes and reads of `phase_a`'s index entries in every piece, of the overview's summary blocks, and of the
    /// zoom's.
    pub index_read: Read,
    pub read: Read,
    pub zoom_read: Read,
    /// Bytes of `phase_a`'s chunks over the hour, which the overview does not read.
    pub sample_bytes: u64,
}

/// The highest magnitude of a node's minimum and maximum, a stored number.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the minimum and maximum of i16 samples: whole numbers well within i64"
)]
fn magnitude(s: &ChannelStats) -> i64 {
    let [lo, _] = s.min;
    let [hi, _] = s.max;
    (lo.as_f64().abs()).max(hi.as_f64().abs()) as i64
}

// --8<-- [start:column_of_item]
/// The column of sample `i` of `total`.
fn column_of(i: u64, total: u64) -> usize {
    let c = u128::from(i) * COLUMNS as u128 / u128::from(total.max(1));
    usize::try_from(c).map_or(COLUMNS - 1, |c| c.min(COLUMNS - 1))
}
// --8<-- [end:column_of_item]

// --8<-- [start:nodes_item]
/// The nodes of the summary blocks of `phase_a` at `level` in every piece that cover any sample of `items`, each with
/// its first sample and the samples it covers.
fn nodes<S: ByteSource>(
    reader: &TapeReader<S>,
    entries: &[Vec<IndexEntry>],
    level: u8,
    items: core::ops::Range<u64>,
    mut take: impl FnMut(u64, u64, ChannelStats),
) -> Result<(u64, u64), FaultError<S::Error>> {
    let (mut blocks, mut count) = (0, 0);
    let mut buf = Vec::new();
    for (i, entries) in entries.iter().enumerate() {
        // --8<-- [start:nodes]
        for e in entries {
            // entries: phase_a's index entries in piece i
            let wanted =
                e.first_item < ItemNum::new(items.end) && e.end_item() > ItemNum::new(items.start);
            if e.kind != BlockKind::Summary || e.level != level || !wanted {
                continue; // another block, another level, or outside the range drawn
            }
            let block = reader.read_block(i, e, &mut buf)?;
            let summary = Summary::of_block(&block).ok_or(FaultError::Missing("summary"))?;
            let span = 1u64 << summary.head().node_shift; // samples a node covers
            for n in 0..summary.node_count() {
                let stats = summary
                    .channel_stats(0, n)
                    .ok_or(FaultError::Missing("node"))?;
                let first = summary.head().first_item.saturating_add(n as u64 * span);
                let samples = span.min(first.saturating_offset_to(e.end_item())); // the last node may be partial
                take(first.get(), samples, stats);
            }
            // --8<-- [end:nodes]
            blocks += 1;
            count += summary.node_count() as u64;
        }
    }
    Ok((blocks, count))
}
// --8<-- [end:nodes_item]

/// Draw the hour of `phase_a` in [COLUMNS] columns from its summaries, then zoom into the column of the highest peak.
pub fn overview(
    reader: &TapeReader<&Counted<'_>>,
    src: &Counted<'_>,
) -> Result<Overview, FaultError<OutOfRange>> {
    let piece = reader
        .pieces()
        .first()
        .ok_or(FaultError::Missing("piece"))?;
    let (_, spec) = series_def(reader, 0, piece.end)?;
    let spec = spec.ok_or(FaultError::Missing("summaries"))?;
    let mut total = 0;
    let mut sample_bytes = 0;
    for p in reader.pieces() {
        let (_, tracks) = p.close.as_ref().ok_or(FaultError::Missing("close"))?;
        for t in tracks.iter().filter(|t| t.track_id == key("phase_a").id()) {
            total = total.max(t.end_item().get());
        }
    }
    let entries = (0..reader.pieces().len())
        .map(|i| reader.track_entries(i, key("phase_a").id()))
        .collect::<Result<Vec<_>, _>>()?;
    for e in entries
        .iter()
        .flatten()
        .filter(|e| e.kind == BlockKind::Series)
    {
        sample_bytes += u64::from(e.len);
    }
    let index_read = src.take();
    // --8<-- [start:level]
    // the coarsest level whose nodes still outnumber the columns: a level up, a node covers 16 times as many samples
    let mut level = 0u8;
    while total >> (spec.base_shift + 4 * (level + 1)) >= COLUMNS as u64 {
        level += 1;
    }
    // --8<-- [end:level]
    let empty = ChannelStats::empty(ElementDesc::of(Element::I16));
    let mut columns: Vec<Option<Column>> = vec![None; COLUMNS];
    let (blocks, count) = nodes(
        reader,
        &entries,
        level,
        0..total,
        |first, samples, stats| {
            if let Some(c) = columns.get_mut(column_of(first, total)) {
                let col = c.get_or_insert(Column {
                    stats: empty,
                    samples: 0,
                });
                col.stats.merge(&stats, col.samples, samples); // every statistic merges
                col.samples += samples;
            }
        },
    )?;
    let read = src.take();
    let (peak_column, peak) = columns
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.map(|c| (i, magnitude(&c.stats))))
        .max_by_key(|&(i, m)| (m, core::cmp::Reverse(i)))
        .ok_or(FaultError::Missing("column"))?;
    let column_us = 60 * US_PER_MINUTE / COLUMNS_I64;
    let column_minute = i64::try_from(peak_column).map_or(i64::MAX, |c| c * column_us);
    // the zoom: level-0 nodes of that column alone
    let mut zoom = (0u64, i64::MIN);
    let column = |c: u64| c * total / COLUMNS as u64;
    let zoomed = column(peak_column as u64)..column(peak_column as u64 + 1);
    nodes(reader, &entries, 0, zoomed, |first, _, stats| {
        if column_of(first, total) == peak_column && magnitude(&stats) > zoom.1 {
            zoom = (first, magnitude(&stats));
        }
    })?;
    let zoom_read = src.take();
    Ok(Overview {
        level,
        node_samples: 1 << (spec.base_shift + 4 * level),
        blocks,
        nodes: count,
        columns,
        peak_column,
        peak: (column_minute, peak),
        zoom: zoom.0,
        index_read,
        read,
        zoom_read,
        sample_bytes,
    })
}

// --8<-- [start:checked_item]
/// What [check] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The chain's links that are [Linked](ww_tape::piece::ChainLink::Linked), and whether it is whole.
    pub linked: usize,
    pub whole: bool,
    /// Each piece holds the content hash its close states.
    pub verified: Vec<bool>,
}
// --8<-- [end:checked_item]

/// Where the bytes go that verifying every piece reads beyond the recording's length.
///
/// Each block is read as its header, then whole. The final index is read whole, then each of its entries again, as the replay merges the track
/// lists. The close record and the tail after it are not read again. [Self::extra] adds them up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyCost {
    /// Blocks read, before the close records: the walk's and the final indexes'.
    pub blocks: u64,
    /// Bytes of the final indexes, and of their entries.
    pub index: u64,
    pub entries: u64,
    /// Bytes of the close records and tails, which the walk stops before.
    pub unread: u64,
}

impl VerifyCost {
    /// Bytes verify reads beyond the recording's length: a header per block, the final index twice, less the
    /// closes and tails.
    pub fn extra(&self) -> u64 {
        let headers = self.blocks * ww_tape::HEADER_LEN as u64;
        headers + self.index + self.entries - self.unread
    }
}

/// What verifying each of `pieces` costs beyond its length ([VerifyCost]), from the pieces' blocks and indexes.
pub fn verify_cost<S: ByteSource>(
    reader: &TapeReader<S>,
    pieces: &[Vec<u8>],
) -> Result<VerifyCost, FaultError<S::Error>> {
    let mut cost = VerifyCost {
        blocks: 0,
        index: 0,
        entries: 0,
        unread: 0,
    };
    for (i, (info, bytes)) in reader.pieces().iter().zip(pieces).enumerate() {
        let close = info.close_offset.ok_or(FaultError::Missing("a close"))?;
        let index = info
            .close
            .as_ref()
            .and_then(|(c, _)| c.final_index)
            .ok_or(FaultError::Missing("a final index"))?;
        let mut walk = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
        let mut at = 0u64;
        while let Some(b) = walk.next_block() {
            let b = b.map_err(FaultError::Walk)?;
            if at < close {
                cost.blocks += 1;
                if b.kind == BlockKind::Index && at >= index {
                    cost.blocks += 1;
                    cost.index += b.len() as u64;
                }
            }
            at += b.len() as u64;
        }
        cost.entries += reader.closed_entries(i)?.len() as u64 * ENTRY_LEN as u64;
        cost.unread += bytes.len() as u64 - close;
    }
    Ok(cost)
}

// --8<-- [start:same_frames_type_item]
/// What [same_frames] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SameFrames {
    /// The frames of minutes 51 to 53: their item numbers, and the chunks the range hash folded, all from the index.
    pub items: core::ops::Range<ItemNum>,
    pub chunks: u64,
    pub stated: u64,
    /// The range hash in the whole chain and in a copy of its last piece alone are equal.
    pub same: bool,
}
// --8<-- [end:same_frames_type_item]

// --8<-- [start:frame_items_item]
/// The item numbers of `can0` whose chunks start within minutes `from..to`, from the index of piece `piece`.
fn frame_items<S: ByteSource>(
    reader: &TapeReader<S>,
    piece: usize,
    minutes: core::ops::Range<i64>,
) -> Result<core::ops::Range<ItemNum>, FaultError<S::Error>> {
    let (from, to) = (minutes.start * US_PER_MINUTE, minutes.end * US_PER_MINUTE);
    let chunks: Vec<IndexEntry> = reader
        .track_entries(piece, key("can0").id())?
        .into_iter()
        .filter(|e| e.kind == BlockKind::Events && from <= e.first_tick && e.first_tick < to)
        .collect();
    let first = chunks.first().ok_or(FaultError::Missing("frames"))?;
    let last = chunks.last().ok_or(FaultError::Missing("frames"))?;
    Ok(first.first_item..last.end_item())
}
// --8<-- [end:frame_items_item]

/// Check the chain's links and every piece's hash.
pub fn check<S: ByteSource>(reader: &TapeReader<S>) -> Result<Checked, FaultError<S::Error>> {
    // --8<-- [start:check]
    let report = reader.chain(); // each head against the piece before it, each close against the next
    let verified = (0..reader.pieces().len())
        .map(|i| reader.verify(i).map(|v| v.holds)) // every byte hashed and replayed
        .collect::<Result<Vec<bool>, _>>()?;
    // --8<-- [end:check]
    let linked = report
        .links
        .iter()
        .filter(|l| matches!(l, ww_tape::piece::ChainLink::Linked))
        .count();
    Ok(Checked {
        linked,
        whole: report.is_whole(),
        verified,
    })
}

/// Compare the range hash of minutes 51 to 53 of `can0` in the chain and in `copy`, a tape of its last piece alone.
pub fn same_frames<S: ByteSource<Error = OutOfRange>>(
    reader: &TapeReader<S>,
    copy: &[u8],
) -> Result<SameFrames, FaultError<OutOfRange>> {
    let last = reader.pieces().len().saturating_sub(1);
    let items = frame_items(reader, last, 51..53)?;
    let copy_reader = TapeReader::open(copy)?; // the last piece alone, as a colleague got it
    // --8<-- [start:range]
    let can0 = key("can0").id();
    let whole = reader.range_hash(can0, OwnerId::NONE, items.clone(), TrackHash::START)?;
    let alone = copy_reader.range_hash(can0, OwnerId::NONE, items.clone(), TrackHash::START)?;
    let same = whole.hash == alone.hash && whole.items == alone.items;
    // --8<-- [end:range]
    Ok(SameFrames {
        items: whole.items.clone(),
        chunks: whole.chunks,
        stated: whole.stated(),
        same,
    })
}

/// Everything Part IV finds, and the bytes each step read.
#[derive(Clone, Debug, PartialEq)]
pub struct Fault {
    /// Bytes of the tape.
    pub len: u64,
    pub opened: Opened,
    pub open_read: Read,
    pub found: Found,
    pub find_read: Read,
    /// The mode in force at each chunk [Found::chunks] names.
    pub modes: Vec<InForce>,
    pub mode_read: Read,
    pub frames: Frames,
    pub frames_read: Read,
    pub current: Current,
    pub current_read: Read,
    pub overview: Overview,
    pub checked: Checked,
    pub check_read: Read,
    pub verify_cost: VerifyCost,
    pub same: SameFrames,
    pub same_read: Read,
}

impl Fault {
    /// Run every step of Part IV on `pieces`, the drive's chain, back to back as one file of the chain holds them.
    pub fn find(pieces: &[Vec<u8>]) -> Result<Self, FaultError<OutOfRange>> {
        let bytes = pieces.concat();
        let src = Counted::new(&bytes);
        let (reader, opened) = open(&src)?;
        let open_read = src.take();
        let found = find(&reader, opened.piece)?;
        let find_read = src.take();
        let modes = mode(&reader, opened.piece, &found.chunks)?;
        let mode_read = src.take();
        let frames = frames(&reader, opened.piece)?;
        let frames_read = src.take();
        let current = current(&reader, opened.piece, &found.chunks)?;
        let current_read = src.take();
        let overview = overview(&reader, &src)?;
        let copy = pieces
            .last()
            .ok_or(FaultError::Missing("piece"))?
            .as_slice();
        _ = src.take();
        let checked = check(&reader)?;
        let check_read = src.take();
        let verify_cost = verify_cost(&reader, pieces)?;
        _ = src.take();
        let same = same_frames(&reader, copy)?;
        let same_read = src.take();
        Ok(Self {
            len: bytes.len() as u64,
            opened,
            open_read,
            found,
            find_read,
            modes,
            mode_read,
            frames,
            frames_read,
            current,
            current_read,
            overview,
            checked,
            check_read,
            verify_cost,
            same,
            same_read,
        })
    }

    /// Whether the frames after the fault start with the emergency frame.
    pub fn emergency_first(&self) -> bool {
        self.frames
            .frames
            .first()
            .is_some_and(|f| f.id == EMERGENCY_ID)
    }
}

/// `m:ss.mmm` of `us` since boot.
pub fn clock(us: i64) -> String {
    let ms = us / 1000;
    format!("{}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

/// What Part IV's program finds at the fault and what each step reads, as `cargo run -p example_drive` prints it.
impl core::fmt::Display for Fault {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let f = self;
        let read = |r: &Read| format!("{} bytes in {} reads", r.bytes, r.reads);
        writeln!(
            out,
            "Finding the fault in the chain as one file of {} bytes:",
            f.len
        )?;
        writeln!(out)?;
        writeln!(out, "| Step | Finds | Reads |")?;
        writeln!(out, "|---|---|---:|")?;
        writeln!(
            out,
            "| open | {} pieces; the fault in piece {} | {} |",
            f.opened.pieces,
            f.opened.piece,
            read(&f.open_read)
        )?;
        let chunks: Vec<String> = f
            .found
            .chunks
            .iter()
            .map(|e| format!("{} to {}", clock(e.first_tick), clock(e.last_tick)))
            .collect();
        writeln!(
            out,
            "| find | {} of phase_a's {} entries: {} | {} |",
            chunks.len(),
            f.found.listed,
            chunks.join(", "),
            read(&f.find_read)
        )?;
        let modes: Vec<String> = f
            .modes
            .iter()
            .map(|m| {
                format!(
                    "{:?} from {}, {} bytes behind{}",
                    String::from_utf8_lossy(&m.item),
                    clock(m.at_us),
                    m.behind,
                    if m.whole { "" } else { ", not whole" }
                )
            })
            .collect();
        writeln!(
            out,
            "| mode | {} | {} |",
            modes.join("; "),
            read(&f.mode_read)
        )?;
        let frames: Vec<String> = f
            .frames
            .frames
            .iter()
            .map(|fr| format!("{} at {} id {:#05x}", fr.item, clock(fr.us), fr.id))
            .collect();
        writeln!(
            out,
            "| frames | of {} in a block of {} bytes: {} | {} |",
            f.frames.in_chunk,
            f.frames.block_len,
            frames.join(", "),
            read(&f.frames_read)
        )?;
        if let Some(fr) = f.frames.frames.first() {
            writeln!(
                out,
                "| emergency frame | data {:02x?}; first after the fault: {} | |",
                fr.data,
                f.emergency_first()
            )?;
        }
        let c = &f.current;
        let peak_us = |i: u64| {
            let us = i128::from(i) * i128::from(c.period.0) / i128::from(c.period.1).max(1);
            i64::try_from(us).unwrap_or(i64::MAX)
        };
        writeln!(
            out,
            "| current | {} samples from {}, period {}/{} µs, scale {}; peak {} mA at {} | {} |",
            c.samples.len(),
            c.first,
            c.period.0,
            c.period.1,
            c.scale,
            c.peak.1,
            clock(peak_us(c.peak.0)),
            read(&f.current_read)
        )?;
        writeln!(
            out,
            "| current's chunk | {} samples in a block of {} bytes | |",
            c.in_chunk, c.block_len
        )?;
        let o = &f.overview;
        writeln!(
            out,
            "| overview | level {} nodes of {} samples: {} blocks, {} nodes, {} columns; peak column {} from {}, {} mA | {} |",
            o.level,
            o.node_samples,
            o.blocks,
            o.nodes,
            o.columns.iter().flatten().count(),
            o.peak_column,
            clock(o.peak.0),
            o.peak.1,
            read(&o.read)
        )?;
        writeln!(
            out,
            "| phase_a's index entries | in {} pieces | {} |",
            f.opened.pieces,
            read(&o.index_read)
        )?;
        writeln!(
            out,
            "| zoom | the level-0 node from sample {} ({}) | {} |",
            o.zoom,
            clock(peak_us(o.zoom)),
            read(&o.zoom_read)
        )?;
        writeln!(
            out,
            "| phase_a's chunks | {} bytes, read by neither | |",
            o.sample_bytes
        )?;
        let k = &f.checked;
        writeln!(
            out,
            "| check | {} links linked, whole {}; pieces verified {:?} | {} |",
            k.linked,
            k.whole,
            k.verified,
            read(&f.check_read)
        )?;
        let v = &f.verify_cost;
        writeln!(
            out,
            "| verify's reads past the chain | {} bytes: {} block headers of {} bytes, the final indexes' {} bytes, their entries' {} bytes again, less {} bytes of closes and tails | |",
            v.extra(),
            v.blocks,
            ww_tape::HEADER_LEN,
            v.index,
            v.entries,
            v.unread
        )?;
        writeln!(
            out,
            "| range hash | can0 items {}..{}: {} chunks, {} from the index; the same in the last piece alone: {} | {} |",
            f.same.items.start,
            f.same.items.end,
            f.same.chunks,
            f.same.stated,
            f.same.same,
            read(&f.same_read)
        )?;

        Ok(())
    }
}
