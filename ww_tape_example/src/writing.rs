//! The numbers of the book's Part III, writing.
//!
//! They are the drive's hour as the recorder's calls and the blocks they make, the same hour in an aligned file, its
//! columns per codec, what the crash at minute 37 loses under each sync policy, the cut between two pieces, the chain
//! with its first piece deleted, and the hour relayed from the board's own tape.
//!
//! [Writing::of] computes them from a recording; its [Display](core::fmt::Display) is what
//! `cargo run -p ww_tape_example` prints from "Writing the drive's hour" on and what `tests/writing.out` pins.

#![expect(
    clippy::cast_precision_loss,
    reason = "ratios and MiB of a recording of a few hundred MiB, printed to two or three decimals"
)]

use core::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ww_tape::index::{IndexBlock, IndexKind};
use ww_tape::piece::{ChainLink, check_chain};
use ww_tape::piece::{TAIL_LEN, close_padding};
use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::{
    Block, BlockKind, Clock, ClockRef, Codec, ColumnKind, FLAG_REPEAT, FileBlocks, Layout,
    MemError, MemSink, MemTape, MemTapes, PieceChain, Recovery, Slot, SyncPolicy, TapeReader,
    TapeSink, TapeWriter, Tapes,
};

use crate::defs::{key, phase_def};
use crate::record::{Act, Recorder, drive_definitions, item_us, options, records_block};
use crate::signals::{US_PER_MS, phase_ma};
use crate::{CHAIN, Drive, Error, OWNER, Recording};

/// What a block of a piece is, as the book names it.
pub fn part(b: &Block<'_>) -> Result<&'static str, Error> {
    Ok(match b.kind {
        BlockKind::PieceHead => "piece head",
        BlockKind::Records if b.flags & FLAG_REPEAT != 0 => "records blocks of puts and copies",
        BlockKind::Records => "records blocks",
        BlockKind::Events => "events chunks",
        BlockKind::Series => "series chunks",
        BlockKind::Summary => "summary blocks",
        BlockKind::Index => match IndexBlock::read(b)?.kind() {
            IndexKind::Checkpoint => "checkpoints",
            IndexKind::Final => "final index",
        },
        BlockKind::Close => "close",
        BlockKind::Tail => "tail",
        _ => "other blocks",
    })
}

/// Whether the writer makes blocks of `part` by itself: puts, checkpoints and a piece's head and end.
fn by_writer(part: &str) -> bool {
    !matches!(
        part,
        "records blocks" | "events chunks" | "series chunks" | "summary blocks"
    )
}

/// The parts of `piece` in file order: what each block is, where it starts and its length.
fn parts(piece: &[u8]) -> Result<Vec<(&'static str, u64, u64)>, Error> {
    let mut out = Vec::new();
    let mut walk = FileBlocks::with_recovery(piece, Recovery::Prefix);
    while let Some(b) = walk.next_block() {
        let b = b?;
        let (name, len) = (part(&b)?, b.len() as u64);
        out.push((name, walk.offset(), len));
    }
    Ok(out)
}

/// The calls the recorder makes in the hour, by kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Calls {
    /// Puts of the mode, and those that wrote a record.
    pub puts: u64,
    pub puts_wrote: u64,
    /// Records blocks of log lines and temperatures.
    pub records: u64,
    /// Chunks of `can0` and of `phase_a`.
    pub frame_chunks: u64,
    pub sample_chunks: u64,
    /// Summary blocks of `phase_a` handed over to wait for a gap.
    pub summaries: u64,
}

/// Blocks of one kind in the whole recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocks {
    pub part: &'static str,
    /// The writer made them by itself.
    pub by_writer: bool,
    pub count: u64,
    pub bytes: u64,
}

/// The hour in an aligned file, every chunk's samples on 4 KiB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Aligned {
    /// Bytes of the recording unaligned and aligned.
    pub unaligned: u64,
    pub aligned: u64,
    /// Summary blocks in the aligned recording, and those placed right before a series chunk.
    pub summaries: u64,
    pub summaries_in_gaps: u64,
    /// Checkpoints in the aligned recording, and those placed right before a series chunk.
    pub checkpoints: u64,
    pub checkpoints_in_gaps: u64,
    /// `phase_a` and its summaries alone in one piece: its bytes unaligned and aligned, its summary blocks and those
    /// placed right before a series chunk.
    pub series_unaligned: u64,
    pub series_aligned: u64,
    pub series_summaries: u64,
    pub series_in_gaps: u64,
}

/// The columns of the hour's chunks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Columns {
    /// Frames on `can0`, its chunks and those whose starts are linear.
    pub frames: u64,
    pub frame_chunks: u64,
    pub linear: u64,
    /// Bits of residuals per start, over every group of every linear chunk, times 1000.
    pub start_millibits: u64,
    /// Bytes of the starts, payload lengths and payloads columns of `can0`.
    pub starts: u64,
    pub lengths: u64,
    pub payloads: u64,
    /// Chunks whose payloads column is lz4.
    pub lz4: u64,
    /// Payload bytes before coding.
    pub payloads_raw: u64,
    /// Samples of `phase_a`, and the bytes of its samples column in each codec.
    pub samples: u64,
    pub codecs: Vec<(&'static str, u64)>,
}

/// What the crash at minute 37 loses under one sync policy: the bytes and blocks written after the last sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncLoss {
    pub policy: SyncPolicy,
    /// Syncs made from the start of the run to the crash.
    pub syncs: u64,
    /// Bytes of the recording past the last sync at the crash, and the whole blocks among them (the torn one aside).
    pub bytes: u64,
    pub blocks: u64,
    /// Milliseconds of the run between the last sync and the crash.
    pub ms: u64,
}

/// The end of one piece and the start of the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cut {
    /// Bytes of piece 0's final index, close (with its padding), padding, and tail.
    pub final_index: u64,
    pub close: u64,
    pub padding: u64,
    pub tail: u64,
    /// Bytes of piece 1's head, and the blocks of puts before its first data and their bytes.
    pub head: u64,
    pub put_blocks: u64,
    pub put_bytes: u64,
    /// Definitions of tracks and of clocks and maps in force at piece 1's first data.
    pub tracks: usize,
    pub clocks: usize,
    /// Each piece's length and its padding.
    pub pieces: Vec<(u64, u64)>,
}

/// The chain read with its first piece deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retention {
    /// The pieces left, the first one's sequence number, the links between them, all linked.
    pub pieces: usize,
    pub first: u64,
    pub linked: usize,
    pub head_cut: bool,
    pub whole: bool,
    /// Track definitions in force at the first data of the oldest piece left.
    pub tracks: usize,
}

/// The hour written by the board as one tape, then relayed by the host into a chain of pieces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relay {
    /// The board's tape: its bytes, data blocks, and its blocks of puts and their bytes.
    pub board_bytes: u64,
    pub data_blocks: u64,
    pub board_puts: u64,
    pub board_put_bytes: u64,
    /// The relay's chain: pieces, bytes, its own blocks of puts and their bytes.
    pub pieces: usize,
    pub relay_bytes: u64,
    pub relay_puts: u64,
    pub relay_put_bytes: u64,
    /// The relay's data blocks are the board's, byte for byte.
    pub same_data: bool,
    /// Bytes of the host's own recording of the hour, for comparison.
    pub recorded_bytes: u64,
}

/// The numbers of Part III.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writing {
    pub drive: Drive,
    pub calls: Calls,
    pub blocks: Vec<Blocks>,
    pub aligned: Aligned,
    pub columns: Columns,
    pub syncs: Vec<SyncLoss>,
    pub cut: Cut,
    pub retention: Retention,
    pub relay: Relay,
}

/// The sync policies the chapter "Files and syncing" compares.
pub const SYNC_POLICIES: [SyncPolicy; 4] = [
    SyncPolicy::Interval(ww_tape::writer::DEFAULT_SYNC_INTERVAL),
    SyncPolicy::EveryBlocks(1),
    SyncPolicy::EveryBlocks(256),
    SyncPolicy::None,
];

impl Writing {
    /// The numbers of `recording`, the drive's run recorded whole. The parts that record the hour again run on
    /// threads of their own.
    pub fn of(recording: &Recording) -> Result<Self, Error> {
        // the acts are the same under every byte policy: made once, shared by every run below
        let acts = recording.drive.acts()?.collect::<Result<Vec<_>, _>>()?;
        let acts = acts.as_slice();
        std::thread::scope(|s| {
            let calls = s.spawn(|| calls_and_columns(recording, acts));
            let aligned = s.spawn(|| aligned(recording, acts));
            let series = s.spawn(|| series_alone(recording, acts));
            let relay = s.spawn(|| relay(recording, acts));
            let syncs: Vec<_> = SYNC_POLICIES
                .iter()
                .map(|p| s.spawn(move || sync_loss(recording, acts, *p)))
                .collect();
            let blocks = blocks(recording)?;
            let cut = cut(recording)?;
            let retention = retention(recording)?;
            let (calls, columns) = joined(calls)?;
            let mut aligned = joined(aligned)?;
            [
                aligned.series_unaligned,
                aligned.series_aligned,
                aligned.series_summaries,
                aligned.series_in_gaps,
            ] = joined(series)?;
            Ok(Self {
                drive: recording.drive,
                calls,
                blocks,
                aligned,
                columns,
                syncs: syncs.into_iter().map(joined).collect::<Result<_, _>>()?,
                cut,
                retention,
                relay: joined(relay)?,
            })
        })
    }
}

/// What a thread of [Writing::of] made; its panic goes on in the caller.
fn joined<T>(h: std::thread::ScopedJoinHandle<'_, Result<T, Error>>) -> Result<T, Error> {
    h.join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The recorder's calls of the hour and the columns of its chunks.
fn calls_and_columns(recording: &Recording, acts: &[Act]) -> Result<(Calls, Columns), Error> {
    let drive = recording.drive;
    let mut calls = Calls {
        puts_wrote: recording
            .made
            .iter()
            .filter(|m| m.put == Some(true))
            .count() as u64,
        ..Calls::default()
    };
    let mut c = Columns::default();
    let codecs = [
        ("raw", Codec::RAW),
        ("delta", Codec::DELTA),
        ("delta2", Codec::DELTA2),
    ];
    let mut codec_bytes = [0u64; 3];
    let mut start_bits = 0u64;
    for act in acts {
        match act {
            Act::Put { .. } => calls.puts += 1,
            Act::Records(_) => calls.records += 1,
            Act::Summary(_) => calls.summaries += 1,
            Act::Chunk { head, columns } if head.track_id == key("can0") => {
                calls.frame_chunks += 1;
                c.frame_chunks += 1;
                c.frames += u64::from(head.count);
                for (spec, bytes) in columns {
                    let len = bytes.len() as u64;
                    match spec.kind {
                        ColumnKind::Starts => {
                            c.starts += len;
                            if head.time_mode == 1 {
                                c.linear += 1;
                                start_bits += residual_bits(bytes, u64::from(head.count));
                            }
                        }
                        ColumnKind::PayloadLengths => c.lengths += len,
                        ColumnKind::Payloads => {
                            c.payloads += len;
                            if spec.codec == Codec::LZ4 {
                                c.lz4 += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Act::Chunk { head, columns } => {
                calls.sample_chunks += 1;
                c.samples += u64::from(head.count);
                let values = phase_values(&drive, head.first_item, u64::from(head.count));
                for ((_, codec), sum) in codecs.iter().zip(&mut codec_bytes) {
                    // the recorder's own codec: its column as it was handed over, not encoded again
                    *sum += match columns.iter().find(|(spec, _)| spec.codec == *codec) {
                        Some((_, bytes)) => bytes.len() as u64,
                        None => sample_bytes(&drive, *codec, &values)?,
                    };
                }
            }
        }
    }
    c.start_millibits = start_bits * 1000 / c.frames.max(1);
    c.payloads_raw = frame_payload_bytes(&drive)?;
    c.codecs = codecs
        .iter()
        .zip(codec_bytes)
        .map(|((name, _), bytes)| (*name, bytes))
        .collect();
    Ok((calls, c))
}

/// Bits of residuals of a linear starts column of `n` items: each group's width times its items.
fn residual_bits(column: &[u8], n: u64) -> u64 {
    const GROUP: u64 = 128;
    const RECORD: usize = 25;
    (0..n.div_ceil(GROUP))
        .map(|g| {
            let width = column
                .get(usize::try_from(g).map_or(usize::MAX, |g| g * RECORD + 20))
                .map_or(0, |w| u64::from(*w));
            width * GROUP.min(n - g * GROUP)
        })
        .sum()
}

/// The samples of `phase_a` of `drive` from item `first`, `n` of them.
pub fn phase_values(drive: &Drive, first: u64, n: u64) -> Vec<i16> {
    let hz = u64::from(drive.phase_hz) * 3_600;
    (first..first + n)
        .map(|i| phase_ma(item_us(i, hz), i, drive.phase_hz))
        .collect()
}

/// Bytes of a samples column of `values` in `codec`.
fn sample_bytes(drive: &Drive, codec: Codec, values: &[i16]) -> Result<u64, Error> {
    let def = phase_def(drive.phase_hz, false).ok_or(Error::Settings("phase_hz is not 0"))?;
    let shape = crate::defs::phase_shape(&def);
    let mut out = vec![0u8; max_encoded_len(codec, shape, values.len())?];
    Ok(encode(codec, shape, values, &mut out)? as u64)
}

/// Bytes of every frame's payload of the hour, before coding.
fn frame_payload_bytes(drive: &Drive) -> Result<u64, Error> {
    let period_us = 1_000_000 / u64::from(drive.can_hz);
    let frames = u64::from(drive.can_hz) * 60 * u64::from(drive.minutes);
    Ok((0..frames)
        .map(|k| crate::signals::frame(k, period_us).1.len() as u64)
        .sum())
}

/// The recording's blocks by kind, in order of first appearance.
fn blocks(recording: &Recording) -> Result<Vec<Blocks>, Error> {
    let mut out: Vec<Blocks> = Vec::new();
    for piece in &recording.pieces {
        for (name, _, len) in parts(piece)? {
            match out.iter_mut().find(|b| b.part == name) {
                Some(b) => {
                    b.count += 1;
                    b.bytes += len;
                }
                None => out.push(Blocks {
                    part: name,
                    by_writer: by_writer(name),
                    count: 1,
                    bytes: len,
                }),
            }
        }
    }
    Ok(out)
}

/// The hour in an aligned file.
fn aligned(recording: &Recording, acts: &[Act]) -> Result<Aligned, Error> {
    let drive = Drive {
        layout: Layout::ALIGNED,
        ..recording.drive
    };
    let aligned = drive.record_acts(acts)?;
    let mut a = Aligned {
        unaligned: recording.len(),
        aligned: aligned.len(),
        summaries: 0,
        summaries_in_gaps: 0,
        checkpoints: 0,
        checkpoints_in_gaps: 0,
        series_unaligned: 0,
        series_aligned: 0,
        series_summaries: 0,
        series_in_gaps: 0,
    };
    for piece in &aligned.pieces {
        let (summaries, in_gaps) = before_series(piece, "summary blocks")?;
        let (checkpoints, cp_in_gaps) = before_series(piece, "checkpoints")?;
        a.summaries += summaries;
        a.summaries_in_gaps += in_gaps;
        a.checkpoints += checkpoints;
        a.checkpoints_in_gaps += cp_in_gaps;
    }
    Ok(a)
}

/// `phase_a` and its summaries of `drive` alone, written into one piece in memory and closed.
pub fn series_tape(drive: &Drive) -> Result<Vec<u8>, Error> {
    let acts = drive.acts()?.collect::<Result<Vec<_>, _>>()?;
    series_tape_of(drive, &acts)
}

/// [series_tape] of `acts`, the acts of `drive`.
fn series_tape_of(drive: &Drive, acts: &[Act]) -> Result<Vec<u8>, Error> {
    let series = |act: &Act| match act {
        Act::Chunk { head, .. } => head.track_id == key("phase_a"),
        Act::Summary(_) => true,
        _ => false,
    };
    one_tape(drive, acts, crate::PRODUCER, series)
}

/// `phase_a` and its summaries alone in one piece: its bytes unaligned and aligned, its summary blocks and those
/// placed right before a series chunk when aligned.
fn series_alone(recording: &Recording, acts: &[Act]) -> Result<[u64; 4], Error> {
    let drive = Drive {
        layout: Layout::ALIGNED,
        ..recording.drive
    };
    let unaligned = series_tape_of(&recording.drive, acts)?;
    let aligned = series_tape_of(&drive, acts)?;
    let (summaries, in_gaps) = before_series(&aligned, "summary blocks")?;
    Ok([
        unaligned.len() as u64,
        aligned.len() as u64,
        summaries,
        in_gaps,
    ])
}

/// Blocks of `part` in `piece`, and those right before a series chunk.
fn before_series(piece: &[u8], part: &str) -> Result<(u64, u64), Error> {
    let parts = parts(piece)?;
    let mut out = (0, 0);
    for (i, (name, ..)) in parts.iter().enumerate() {
        if *name == part {
            out.0 += 1;
            out.1 += u64::from(
                parts
                    .get(i + 1)
                    .is_some_and(|(next, ..)| *next == "series chunks"),
            );
        }
    }
    Ok(out)
}

/// The run's time, as the host sees it while it records: the time of the latest data handed over.
#[derive(Clone, Default)]
struct RunClock(Arc<AtomicU64>);

impl Clock for RunClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// What the sinks of a [SyncTapes] did: syncs, and where the last one left the piece it synced.
#[derive(Default)]
struct SyncLog {
    syncs: AtomicU64,
    /// The piece of the last sync and its bytes then.
    key: AtomicU64,
    size: AtomicU64,
    /// Blocks written since the last sync.
    writes: AtomicU64,
    /// The run's time at the last sync.
    at_ms: AtomicU64,
    /// Every sync: the piece and its bytes then.
    all: std::sync::Mutex<Vec<(u64, u64)>>,
}

/// [MemTapes] whose sinks log their syncs.
#[derive(Clone)]
struct SyncTapes {
    inner: MemTapes,
    log: Arc<SyncLog>,
    clock: RunClock,
}

/// A [MemSink] that logs its syncs.
struct SyncSink {
    inner: MemSink,
    key: u64,
    log: Arc<SyncLog>,
    clock: RunClock,
}

impl TapeSink for SyncSink {
    type Error = MemError;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), MemError> {
        self.log.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write_all(bytes)
    }

    fn sync(&mut self) -> Result<(), MemError> {
        let log = &self.log;
        log.syncs.fetch_add(1, Ordering::Relaxed);
        log.key.store(self.key, Ordering::Relaxed);
        log.size.store(self.inner.size()?, Ordering::Relaxed);
        log.writes.store(0, Ordering::Relaxed);
        log.at_ms.store(self.clock.now_ms(), Ordering::Relaxed);
        log.all
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((self.key, self.inner.size()?));
        self.inner.sync()
    }

    fn close(&mut self) -> Result<(), MemError> {
        self.inner.close()
    }

    fn size(&self) -> Result<u64, MemError> {
        self.inner.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        self.inner.read_at(offset, buf)
    }

    fn truncate(&mut self, len: u64) -> Result<(), MemError> {
        self.inner.truncate(len)
    }

    fn reserve(&mut self, bytes: u64) {
        self.inner.reserve(bytes);
    }

    fn base(&self) -> u64 {
        self.inner.base()
    }

    fn holds_pieces(&self) -> bool {
        self.inner.holds_pieces()
    }

    fn rebase(&mut self, base: u64) -> Result<(), MemError> {
        self.inner.rebase(base)
    }
}

impl SyncTapes {
    fn wrap(&self, key: u64, inner: MemSink) -> SyncSink {
        SyncSink {
            inner,
            key,
            log: Arc::clone(&self.log),
            clock: self.clock.clone(),
        }
    }
}

impl Tapes for SyncTapes {
    type Key = u64;
    type Error = MemError;
    type Sink = SyncSink;
    type Source = MemTape;

    fn key_of(&self, seq: u64) -> u64 {
        self.inner.key_of(seq)
    }

    fn create(&mut self, key: &u64) -> Result<SyncSink, MemError> {
        let sink = Tapes::create(&mut self.inner, key)?;
        Ok(self.wrap(*key, sink))
    }

    fn reopen(&mut self, key: &u64) -> Result<Option<SyncSink>, MemError> {
        Ok(Tapes::reopen(&mut self.inner, key)?.map(|s| self.wrap(*key, s)))
    }

    fn closed(&self, key: &u64) -> Result<Option<MemTape>, MemError> {
        Tapes::closed(&self.inner, key)
    }

    fn create_whole(&mut self, key: &u64) -> Result<SyncSink, MemError> {
        let sink = Tapes::create_whole(&mut self.inner, key)?;
        Ok(self.wrap(*key, sink))
    }

    fn open_whole(&mut self, key: &u64) -> Result<Option<SyncSink>, MemError> {
        Ok(Tapes::open_whole(&mut self.inner, key)?.map(|s| self.wrap(*key, s)))
    }
}

/// When the host has the data of `act`: the board's µs time of its latest item, in ms; None for a summary block.
fn act_ms(act: &Act) -> Option<u64> {
    let us = match act {
        Act::Put { ticks, .. } => *ticks,
        Act::Records(records) => records.last()?.1,
        Act::Chunk { head, .. } => head.last_tick,
        Act::Summary(_) => return None,
    };
    u64::try_from(us / US_PER_MS).ok()
}

/// Record the run under `policy` with the run's own clock up to the story's crash, and what the crash loses.
fn sync_loss(recording: &Recording, acts: &[Act], policy: SyncPolicy) -> Result<SyncLoss, Error> {
    let drive = recording.drive;
    let crash = recording.story_crash_byte()?;
    let (mut r, tapes, clock) = logged(&drive, policy)?;
    let log = &tapes.log;
    // what the sinks had synced before the act whose write the crash tears
    let mut before = Synced::of(log);
    for act in acts {
        if let Some(ms) = act_ms(act) {
            clock.0.fetch_max(ms, Ordering::Relaxed);
        }
        before = Synced::of(log);
        r.apply(act)?;
        if r.bytes()? > crash {
            break;
        }
    }
    let pieces_before: u64 = recording
        .closed
        .iter()
        .take(usize::try_from(before.key).unwrap_or(usize::MAX))
        .map(|p| p.piece.len)
        .sum();
    Ok(SyncLoss {
        policy,
        syncs: before.syncs,
        bytes: crash.saturating_sub(pieces_before + before.size),
        // the blocks written since, and the torn one
        blocks: before.writes,
        ms: clock.now_ms().saturating_sub(before.at_ms),
    })
}

/// A recorder of `drive` under `policy` whose sinks log their syncs, on the run's own clock, moved to each act's time.
fn logged(
    drive: &Drive,
    policy: SyncPolicy,
) -> Result<(Recorder<SyncTapes>, SyncTapes, RunClock), Error> {
    let clock = RunClock::default();
    let tapes = SyncTapes {
        inner: MemTapes::new(),
        log: Arc::default(),
        clock: clock.clone(),
    };
    let mut o = options(drive);
    o.writer.sync = policy;
    o.writer.clock = ClockRef::new(clock.clone());
    let (defs, numbers) = drive_definitions(drive)?;
    let r = Recorder {
        chain: PieceChain::create_on(tapes.clone(), OWNER, drive.chain, &[], o)?,
        defs,
        numbers,
    };
    Ok((r, tapes, clock))
}

/// Where the writer synced while it recorded `drive` whole under `policy`, on the run's own clock: the bytes of the
/// recording then, every piece counted.
pub fn sync_offsets(drive: &Drive, policy: SyncPolicy) -> Result<Vec<u64>, Error> {
    let (mut r, tapes, clock) = logged(drive, policy)?;
    for act in drive.acts()? {
        let act = act?;
        if let Some(ms) = act_ms(&act) {
            clock.0.fetch_max(ms, Ordering::Relaxed);
        }
        r.apply(&act)?;
    }
    let closed = r.chain.close()?;
    let starts: Vec<u64> = closed
        .iter()
        .scan(0, |at, p| {
            let start = *at;
            *at += p.piece.len;
            Some(start)
        })
        .collect();
    tapes
        .log
        .all
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(key, size)| {
            let start = usize::try_from(*key).ok().and_then(|k| starts.get(k));
            start
                .map(|s| s + size)
                .ok_or(Error::Settings("a sync of a piece of the chain"))
        })
        .collect()
}

/// A copy of a [SyncLog] at one moment.
struct Synced {
    syncs: u64,
    key: u64,
    size: u64,
    writes: u64,
    at_ms: u64,
}

impl Synced {
    fn of(log: &SyncLog) -> Self {
        Self {
            syncs: log.syncs.load(Ordering::Relaxed),
            key: log.key.load(Ordering::Relaxed),
            size: log.size.load(Ordering::Relaxed),
            writes: log.writes.load(Ordering::Relaxed),
            at_ms: log.at_ms.load(Ordering::Relaxed),
        }
    }
}

/// Track definitions and clock and map definitions in force at `offset` of `piece`.
fn definitions_at(piece: &[u8], offset: u64) -> Result<(usize, usize), Error> {
    let r = TapeReader::open(piece)?;
    let defs = r.definitions_at(0, offset)?;
    let tracks = defs
        .items
        .keys()
        .filter(|s| matches!(s, Slot::Track { .. }))
        .count();
    let clocks = defs
        .items
        .keys()
        .filter(|s| matches!(s, Slot::Clock { .. } | Slot::Map { .. }))
        .count();
    Ok((tracks, clocks))
}

/// Where the first block of data of `piece` starts.
fn first_data(piece: &[u8]) -> Result<u64, Error> {
    parts(piece)?
        .into_iter()
        .find(|(name, ..)| !by_writer(name))
        .map(|(_, at, _)| at)
        .ok_or(Error::Settings("a piece holds data"))
}

/// The cut between pieces 0 and 1, and each piece's padding.
fn cut(recording: &Recording) -> Result<Cut, Error> {
    let piece = |seq: usize| {
        recording
            .pieces
            .get(seq)
            .ok_or(Error::Settings("the run has two pieces"))
    };
    let (p0, p1) = (piece(0)?, piece(1)?);
    let len = |parts: &[(&str, u64, u64)], name: &str| {
        parts
            .iter()
            .filter(|(n, ..)| *n == name)
            .map(|(.., l)| l)
            .sum::<u64>()
    };
    let end = parts(p0)?;
    let start = parts(p1)?;
    let head = len(&start, "piece head");
    let puts: Vec<_> = start
        .iter()
        .skip(1)
        .take_while(|(name, ..)| *name == "records blocks of puts and copies")
        .collect();
    let (tracks, clocks) = definitions_at(p1, first_data(p1)?)?;
    let pieces = recording
        .pieces
        .iter()
        .map(|p| Ok((p.len() as u64, padding(p)?)))
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(Cut {
        final_index: len(&end, "final index"),
        close: len(&end, "close"),
        padding: padding(p0)?,
        tail: len(&end, "tail"),
        head,
        put_blocks: puts.len() as u64,
        put_bytes: puts.iter().map(|(.., l)| l).sum(),
        tracks,
        clocks,
        pieces,
    })
}

/// The zeros that pad the close of a closed `piece` to its 4 KiB end.
fn padding(piece: &[u8]) -> Result<u64, Error> {
    let r = TapeReader::open(piece)?;
    let info = r
        .pieces()
        .first()
        .ok_or(Error::Settings("a closed piece"))?;
    let (at, (_, ranges)) = info
        .close_offset
        .zip(info.close.as_ref())
        .ok_or(Error::Settings("a closed piece"))?;
    let pad = close_padding(at, ranges.len(), info.removed.len()) as u64;
    debug_assert_eq!(
        (at + ww_tape::piece::close_len(ranges.len(), info.removed.len()) as u64
            + pad
            + TAIL_LEN as u64)
            % u64::from(ww_tape::PIECE_ALIGN),
        0,
        "the padding ends the piece on 4 KiB"
    );
    Ok(pad)
}

/// The chain read with piece 0 deleted.
fn retention(recording: &Recording) -> Result<Retention, Error> {
    let rest = recording
        .pieces
        .get(1..)
        .ok_or(Error::Settings("the run has two pieces"))?;
    let mut summaries = Vec::new();
    for p in rest {
        let r = TapeReader::open(&p[..])?;
        summaries.extend(r.pieces().first().and_then(|i| i.summary()));
    }
    let report = check_chain(&summaries);
    let first = rest
        .first()
        .ok_or(Error::Settings("the run has two pieces"))?;
    let (tracks, _) = definitions_at(first, first_data(first)?)?;
    Ok(Retention {
        pieces: summaries.len(),
        first: summaries.first().map_or(0, |s| s.ident.seq),
        linked: report
            .links
            .iter()
            .filter(|l| matches!(l, ChainLink::Linked))
            .count(),
        head_cut: report.head_cut,
        whole: report.is_whole(),
        tracks,
    })
}

/// The data blocks of `bytes`: every block but puts, checkpoints and the head and end of a piece.
fn data_blocks(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut out = Vec::new();
    let mut walk = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    while let Some(b) = walk.next_block() {
        let b = b?;
        if !by_writer(part(&b)?) {
            out.push(b.bytes().to_vec());
        }
    }
    Ok(out)
}

/// Blocks of puts in `bytes` and their bytes.
fn put_blocks(bytes: &[u8]) -> Result<(u64, u64), Error> {
    let mut out = (0, 0);
    let mut walk = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    while let Some(b) = walk.next_block() {
        let b = b?;
        if b.kind == BlockKind::Records && b.flags & FLAG_REPEAT != 0 {
            out.0 += 1;
            out.1 += b.len() as u64;
        }
    }
    Ok(out)
}

/// The board writes its hour as one tape; the host relays it into a chain.
fn relay(recording: &Recording, acts: &[Act]) -> Result<Relay, Error> {
    let drive = recording.drive;
    let board = board_tape(&drive, acts)?;
    // --8<-- [start:relay]
    let mut defs = ww_tape::Definitions::new(); // filled from the board's own puts
    let tapes = MemTapes::new();
    let mut chain = PieceChain::create_on(tapes.clone(), OWNER, CHAIN, &[], options(&drive))?;
    let mut walk = FileBlocks::with_recovery(&board[..], Recovery::Prefix);
    while let Some(b) = walk.next_block() {
        let b = b?;
        if ww_tape::index::is_structural(b.kind) {
            continue; // the board's index, close and tail: the relay writes its own
        }
        if matches!(b.kind, BlockKind::Events | BlockKind::Series) && chain.cut_due() {
            chain.cut_now(&defs)?; // a new piece every 15 minutes, as the host's own recording
        }
        chain.writer()?.relay_block(&mut defs, b.bytes())?;
    }
    let closed = chain.close()?;
    // --8<-- [end:relay]
    let pieces = closed
        .iter()
        .map(|p| {
            tapes
                .get(&p.tape)?
                .ok_or(Error::Settings("a closed piece"))?
                .to_vec()
                .map_err(Error::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let relayed = pieces.concat();
    let (board_puts, board_put_bytes) = put_blocks(&board)?;
    let (relay_puts, relay_put_bytes) = put_blocks(&relayed)?;
    let board_data = data_blocks(&board)?;
    Ok(Relay {
        board_bytes: board.len() as u64,
        data_blocks: board_data.len() as u64,
        board_puts,
        board_put_bytes,
        pieces: pieces.len(),
        relay_bytes: relayed.len() as u64,
        relay_puts,
        relay_put_bytes,
        same_data: data_blocks(&relayed)? == board_data,
        recorded_bytes: recording.len(),
    })
}

// --8<-- [start:board_tape_item]
/// The board's own tape of the hour: one piece in memory, closed, its definitions near 1 MiB.
fn board_tape(drive: &Drive, acts: &[Act]) -> Result<Vec<u8>, Error> {
    let board = Drive {
        near_bytes: 1 << 20,
        ..*drive
    };
    one_tape(&board, acts, "drive firmware", |_| true)
}
// --8<-- [end:board_tape_item]

/// The acts of `drive` that `keep` takes, written by one writer into one piece in memory under the drive's options,
/// closed.
fn one_tape(
    drive: &Drive,
    acts: &[Act],
    producer: &str,
    keep: impl Fn(&Act) -> bool,
) -> Result<Vec<u8>, Error> {
    let (defs, numbers) = drive_definitions(drive)?;
    let o = options(drive).writer;
    let sink = MemSink::new();
    let tape = sink.tape();
    let mut w = TapeWriter::create_on(sink, SyncPolicy::None);
    w.set_clock(ClockRef::NONE);
    w.set_layout(o.layout)?;
    w.set_producer(producer);
    w.set_index_policy(o.index)?;
    w.set_near_policy(o.near);
    for act in acts.iter().filter(|a| keep(a)) {
        match act {
            Act::Put { ticks, mode } => {
                w.put(&defs, numbers.mode, *ticks, mode.item())?;
            }
            Act::Records(records) => {
                let block = records_block(w.next_id(), numbers.board_clock, records)?;
                w.write_block(&defs, &block)?;
            }
            Act::Chunk { head, columns } => {
                let columns: Vec<_> = columns.iter().map(|(s, b)| (*s, &b[..])).collect();
                w.write_chunk(&defs, head, &columns)?;
            }
            Act::Summary(block) => w.defer_block(&defs, block.clone())?,
        }
    }
    w.close()?;
    Ok(tape.to_vec()?)
}

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / f64::from(1 << 20))
}

fn percent(part: u64, whole: u64) -> String {
    format!("{:.2} %", part as f64 * 100.0 / whole.max(1) as f64)
}

/// `bytes` rounded for prose: three significant digits at most, in B, kB, MB or GB.
fn round_bytes(bytes: u64) -> String {
    let units = [("GB", 1e9), ("MB", 1e6), ("kB", 1e3)];
    let b = bytes as f64;
    units.iter().find(|(_, n)| b >= *n).map_or_else(
        || format!("{bytes} B"),
        |(u, n)| format!("{} {u}", round_sig(b / n)),
    )
}

/// A count rounded for prose: exact below 1000, else to three significant digits.
fn round_count(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let digits = n.ilog10();
    let step = 10u64.pow(digits.saturating_sub(2));
    format!("about {}", (n + step / 2) / step * step)
}

/// A share rounded for prose: one decimal below 10 %, whole percent above.
fn round_percent(part: u64, whole: u64) -> String {
    format!("{} %", round_sig(part as f64 * 100.0 / whole.max(1) as f64))
}

/// One decimal below 10, a whole number above.
fn round_sig(v: f64) -> String {
    if v < 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    }
}

fn policy(p: SyncPolicy) -> String {
    match p {
        SyncPolicy::Interval(t) => format!("Interval({} s)", t.as_secs()),
        SyncPolicy::EveryBlocks(n) => format!("EveryBlocks({n})"),
        SyncPolicy::None => "None".to_owned(),
    }
}

impl fmt::Display for Writing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = &self.calls;
        writeln!(f, "Writing the drive's hour:")?;
        writeln!(f)?;
        writeln!(f, "| Call | Count |")?;
        writeln!(f, "|---|---:|")?;
        writeln!(f, "| put, the mode | {} ({} wrote) |", c.puts, c.puts_wrote)?;
        writeln!(
            f,
            "| write_block, log lines and temperatures | {} |",
            c.records
        )?;
        writeln!(f, "| write_chunk, can0 | {} |", c.frame_chunks)?;
        writeln!(f, "| write_chunk, phase_a | {} |", c.sample_chunks)?;
        writeln!(f, "| defer_block, summaries of phase_a | {} |", c.summaries)?;
        writeln!(f)?;
        writeln!(f, "| Blocks | Made by | Count | Bytes |")?;
        writeln!(f, "|---|---|---:|---:|")?;
        let total: u64 = self.blocks.iter().map(|b| b.bytes).sum();
        for b in &self.blocks {
            let by = if b.by_writer {
                "the writer"
            } else {
                "the recorder"
            };
            writeln!(f, "| {} | {by} | {} | {} |", b.part, b.count, b.bytes)?;
        }
        let writer: u64 = self
            .blocks
            .iter()
            .filter(|b| b.by_writer)
            .map(|b| b.bytes)
            .sum();
        writeln!(f)?;
        writeln!(
            f,
            "The writer's own blocks: {writer} of {total} bytes, {}.",
            percent(writer, total)
        )?;

        let a = &self.aligned;
        writeln!(f)?;
        writeln!(
            f,
            "Aligned: {} bytes against {} unaligned, {} more ({}).",
            a.aligned,
            a.unaligned,
            a.aligned - a.unaligned,
            percent(a.aligned - a.unaligned, a.unaligned)
        )?;
        writeln!(
            f,
            "In the aligned file {} of {} summary blocks and {} of {} checkpoints lie right before a series chunk.",
            a.summaries_in_gaps, a.summaries, a.checkpoints_in_gaps, a.checkpoints
        )?;
        writeln!(
            f,
            "phase_a alone: {} bytes aligned against {} unaligned, {} more ({}); {} of {} summary blocks lie right before a series chunk.",
            a.series_aligned,
            a.series_unaligned,
            a.series_aligned - a.series_unaligned,
            percent(a.series_aligned - a.series_unaligned, a.series_unaligned),
            a.series_in_gaps,
            a.series_summaries
        )?;

        let k = &self.columns;
        writeln!(f)?;
        writeln!(
            f,
            "can0: {} frames in {} chunks, {} with linear starts; residuals {}.{:03} bits per start.",
            k.frames,
            k.frame_chunks,
            k.linear,
            k.start_millibits / 1000,
            k.start_millibits % 1000
        )?;
        let per = |b: u64| format!("{:.2}", b as f64 / k.frames.max(1) as f64);
        writeln!(
            f,
            "can0 bytes per frame: starts {}, payload lengths {}, payloads {} ({} before coding; {} of {} chunks lz4).",
            per(k.starts),
            per(k.lengths),
            per(k.payloads),
            per(k.payloads_raw),
            k.lz4,
            k.frame_chunks
        )?;
        writeln!(f)?;
        writeln!(f, "| phase_a codec | Bytes | Bytes per sample |")?;
        writeln!(f, "|---|---:|---:|")?;
        for (name, bytes) in &k.codecs {
            writeln!(
                f,
                "| {name} | {bytes} | {:.3} |",
                *bytes as f64 / k.samples.max(1) as f64
            )?;
        }

        writeln!(f)?;
        writeln!(f, "The crash at minute 37, by sync policy:")?;
        writeln!(f)?;
        writeln!(
            f,
            "| Policy | Syncs | Bytes not synced | Whole blocks not synced | Of the run |"
        )?;
        writeln!(f, "|---|---:|---:|---:|---:|")?;
        for s in &self.syncs {
            writeln!(
                f,
                "| {} | {} | {} | {} | {}.{:03} s |",
                policy(s.policy),
                s.syncs,
                s.bytes,
                s.blocks,
                s.ms / 1000,
                s.ms % 1000
            )?;
        }

        let t = &self.cut;
        writeln!(f)?;
        writeln!(
            f,
            "The cut after piece 0: final index {} bytes, close {} bytes ({} of them padding), tail {} bytes.",
            t.final_index, t.close, t.padding, t.tail
        )?;
        writeln!(
            f,
            "Piece 1 starts: head {} bytes, then {} blocks of puts, {} bytes; at its first data {} tracks and {} clocks and maps are defined.",
            t.head, t.put_blocks, t.put_bytes, t.tracks, t.clocks
        )?;
        let pads: Vec<String> = t
            .pieces
            .iter()
            .enumerate()
            .map(|(i, (len, pad))| format!("piece {i} {len} bytes, padding {pad}"))
            .collect();
        writeln!(f, "Pieces: {}.", pads.join("; "))?;

        let r = &self.retention;
        writeln!(f)?;
        writeln!(
            f,
            "Piece 0 deleted: {} pieces from piece {}, {} links linked; head cut {}, whole {}; {} tracks defined at the first data.",
            r.pieces, r.first, r.linked, r.head_cut, r.whole, r.tracks
        )?;

        let y = &self.relay;
        writeln!(f)?;
        writeln!(
            f,
            "The board's own tape: {} bytes, {} data blocks, {} blocks of puts ({} bytes).",
            y.board_bytes, y.data_blocks, y.board_puts, y.board_put_bytes
        )?;
        writeln!(
            f,
            "Relayed: {} pieces, {} bytes ({}), {} blocks of puts ({} bytes); data blocks the board's byte for byte: {}.",
            y.pieces,
            y.relay_bytes,
            mib(y.relay_bytes),
            y.relay_puts,
            y.relay_put_bytes,
            y.same_data
        )?;

        // the same numbers as the chapters' prose gives them, rounded; the exact ones above stay for the tables
        let puts: u64 = self
            .blocks
            .iter()
            .filter(|b| b.by_writer && b.part.contains("puts"))
            .map(|b| b.bytes)
            .sum();
        let every = self
            .syncs
            .iter()
            .find(|s| s.policy == SyncPolicy::EveryBlocks(1))
            .map_or(0, |s| s.syncs);
        writeln!(f)?;
        writeln!(f, "Rounded for the text:")?;
        writeln!(
            f,
            "- the writer's own blocks: {} of {}, {}; its puts {}",
            round_bytes(writer),
            round_bytes(total),
            round_percent(writer, total),
            round_bytes(puts)
        )?;
        writeln!(
            f,
            "- aligned: {} against {}, {} more; phase_a alone {} against {}, {} more",
            round_bytes(a.aligned),
            round_bytes(a.unaligned),
            round_percent(a.aligned - a.unaligned, a.unaligned),
            round_bytes(a.series_aligned),
            round_bytes(a.series_unaligned),
            round_percent(a.series_aligned - a.series_unaligned, a.series_unaligned)
        )?;
        writeln!(
            f,
            "- a sync after every block: {} syncs; piece 0's final index {}",
            round_count(every),
            round_bytes(t.final_index)
        )?;
        writeln!(
            f,
            "- the board's tape: {}, {} data blocks, {} of puts; relayed: {}, {} of puts",
            round_bytes(y.board_bytes),
            round_count(y.data_blocks),
            round_bytes(y.board_put_bytes),
            round_bytes(y.relay_bytes),
            round_bytes(y.relay_put_bytes)
        )?;
        writeln!(f, "The host's own recording: {} bytes.", y.recorded_bytes)
    }
}
