//! The recorder: the acts of the run block by block, written into a chain of pieces in memory, a crash at a byte
//! and the resume.

use ww_tape::ItemNum;

use std::collections::{BTreeMap, VecDeque};
use ww_tape::{ClockNum, TrackNum};

use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::payloads::EncodedPayloads;
use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::times::EncodedTimes;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ChainOptions, ChainPiece, ClockRef, ClockTime, Codec,
    ColumnKind, ColumnSpec, CutPolicy, Definitions, EndMode, FileBlocks, MemError, MemPieces,
    PieceStore, PieceTapes, Recovery, Resumed, SeriesGrid, SessionTag, StatePolicy, SyncPolicy,
    TapeReader, TapeSink, WriterOptions,
};
use ww_tape_mux::{
    ChunkSender, GridHandle, GridOptions, MuxOptions, ProgressWatch, RecordsHandle, StateHandle,
    TapeMux,
};
use ww_tape_summary::{SeriesStart, SeriesSummaries};

use crate::defs::{DriveNumbers, define_on_mux, definitions, key};
use crate::signals::{
    CRASH_MINUTE, FAULT_MINUTE, Mode, US_PER_HOUR, US_PER_MS, emergency_frame, frame, log_line,
    temp_centi,
};
use crate::writing::{chunk_values, phase_values};
use crate::{Drive, Error, OWNER, PRODUCER};

/// The codec of `phase_a`'s samples: differences, bit-packed in groups.
pub(crate) const PHASE_CODEC: Codec = Codec::DELTA;
/// Bytes a record of a records block takes past its item, at most: its head, time and length.
const RECORD_OVERHEAD_MAX: usize = 32;

/// Why [Drive::crash] refuses a byte: the recording ends before it.
pub const CRASH_PAST_END: &str = "the crash byte lies past the end of the recording";

// --8<-- [start:act_item]
/// One thing the recorder hands the chain.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// A put of the mode in force from `ticks` on.
    Put { ticks: i64, mode: Mode },
    /// A records block: log lines and temperatures, (track, ticks, item) in time order.
    Records(Vec<(TrackNum, i64, Vec<u8>)>),
    /// A chunk of `can0` or `phase_a`: its head and columns.
    Chunk {
        head: Box<ChunkHead>,
        columns: Vec<(ColumnSpec, Vec<u8>)>,
    },
    /// A summary block of `phase_a`.
    Summary(Vec<u8>),
}
// --8<-- [end:act_item]

/// The acts of a run in order: for each block of [Drive::block_ms] the puts of the mode, a records block, a chunk of
/// frames, a chunk of samples and the summary blocks those samples finish; the last summaries at the end.
struct Acts {
    drive: Drive,
    /// The numbers of the drive's clocks and tracks
    numbers: DriveNumbers,
    /// The next block of the run
    block: u64,
    blocks: u64,
    summaries: Option<SeriesSummaries<i16>>,
    queue: VecDeque<Act>,
    finished: bool,
    /// Whether the acts are for a mux, which encodes the samples and makes the summaries itself: a chunk of `phase_a`
    /// then holds its head only and no summary acts are made.
    lean: bool,
}

/// The first item of a track of `per_hour` items, item `i` at `i * US_PER_HOUR / per_hour` µs, at or after `us`.
pub(crate) fn first_at(us: i64, per_hour: u64) -> u64 {
    let us = u128::from(us.unsigned_abs());
    let n = (us * u128::from(per_hour)).div_ceil(u128::from(US_PER_HOUR));
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The time of item `i` of a track of `per_hour` items, µs.
pub(crate) fn item_us(i: u64, per_hour: u64) -> i64 {
    let us = u128::from(i) * u128::from(US_PER_HOUR) / u128::from(per_hour.max(1));
    i64::try_from(us).unwrap_or(i64::MAX)
}

impl Acts {
    /// The acts of `drive` for a writer that takes every block made, as [Drive::acts] says.
    fn new(drive: Drive) -> Result<Self, Error> {
        Self::make(drive, false)
    }

    /// The acts of `drive` for a mux: its `phase_a` chunks carry no columns.
    fn lean(drive: Drive) -> Result<Self, Error> {
        Self::make(drive, true)
    }

    fn make(drive: Drive, lean: bool) -> Result<Self, Error> {
        if drive.phase_hz == 0 || 1_000_000 % drive.phase_hz != 0 {
            return Err(Error::Settings("phase_hz divides 1 000 000"));
        }
        if drive.can_hz == 0 || 1_000_000 % drive.can_hz != 0 {
            return Err(Error::Settings("can_hz divides 1 000 000"));
        }
        if drive.block_ms == 0 || 60_000 % drive.block_ms != 0 {
            return Err(Error::Settings("block_ms divides a minute"));
        }
        let numbers = drive.numbers()?;
        let run_ms = u64::from(drive.minutes) * 60_000;
        let blocks = run_ms.div_ceil(u64::from(drive.block_ms));
        let summaries = if drive.summaries && !lean {
            let def = crate::defs::phase_def(drive.phase_hz, true)
                .ok_or(Error::Settings("phase_hz is not 0"))?;
            // --8<-- [start:summaries]
            let start = SeriesStart {
                track_id: key("phase_a").id(),
                track_number: numbers.phase_a,
                shape: crate::defs::phase_shape(&def),
                first_item: ItemNum::new(0),
                grid: def.first_grid(0),
                period: def.period,
                tile: ww_tape::TileCoords::NONE,
                tile_cells: 1,
            };
            Some(SeriesSummaries::new(start, crate::defs::phase_summaries())?)
            // --8<-- [end:summaries]
        } else {
            None
        };
        Ok(Self {
            drive,
            numbers,
            block: 0,
            blocks,
            summaries,
            queue: VecDeque::new(),
            finished: false,
            lean,
        })
    }

    /// Queue the acts of the next block, or the last summaries after the last block.
    fn fill(&mut self) -> Result<(), Error> {
        if self.block >= self.blocks {
            if !self.finished {
                self.finished = true;
                if let Some(s) = &mut self.summaries {
                    s.finish()?;
                    self.queue
                        .extend(s.take_blocks().into_iter().map(Act::Summary));
                }
            }
            return Ok(());
        }
        let d = self.drive;
        let end_us = i64::from(d.minutes) * 60_000 * US_PER_MS;
        let block_us = i64::from(d.block_ms) * US_PER_MS;
        let i = i64::try_from(self.block).map_err(|_e| Error::Settings("too many blocks"))?;
        let t0 = i * block_us;
        let t1 = (t0 + block_us).min(end_us);
        self.block += 1;

        // the modes in force in the block, each from where it starts
        let mut mode = Mode::at(t0);
        self.queue.push_back(Act::Put { ticks: t0, mode });
        while let Some(next) = mode.next_change_us().filter(|&c| c < t1) {
            mode = Mode::at(next);
            self.queue.push_back(Act::Put { ticks: next, mode });
        }

        // log lines and temperatures
        let mut records = Vec::new();
        let (log, temp) = (u64::from(d.log_per_hour), u64::from(d.temp_per_hour));
        if log > 0 {
            for n in first_at(t0, log)..first_at(t1, log) {
                let us = item_us(n, log);
                records.push((
                    self.numbers.log,
                    us,
                    log_line(n, us / US_PER_MS).into_bytes(),
                ));
            }
        }
        if temp > 0 {
            for n in first_at(t0, temp)..first_at(t1, temp) {
                let us = item_us(n, temp);
                records.push((
                    self.numbers.temp,
                    us,
                    temp_centi(us / US_PER_MS).to_le_bytes().to_vec(),
                ));
            }
        }
        records.sort_by_key(|&(track, us, _)| (us, track));
        if !records.is_empty() {
            self.queue.push_back(Act::Records(records));
        }

        // CAN frames
        let can = u64::from(d.can_hz) * 3_600;
        let period_us = 1_000_000 / u64::from(d.can_hz);
        let frames = first_at(t0, can)..first_at(t1, can);
        if !frames.is_empty() {
            let (starts, payloads): (Vec<i64>, Vec<Vec<u8>>) =
                frames.clone().map(|k| frame(k, period_us)).unzip();
            // --8<-- [start:frames]
            let times = EncodedTimes::encode(&starts, None, None)?; // instants, no flags
            let payloads = EncodedPayloads::encode(payloads.iter().map(Vec::as_slice))?;
            let head = times.head.chunk_head(
                key("can0").id(),
                self.numbers.can0,
                ItemNum::new(frames.start),
            );
            let columns = times
                .columns()
                .chain(payloads.columns())
                .map(|(spec, bytes)| (spec, bytes.to_vec()))
                .collect();
            // --8<-- [end:frames]
            self.queue.push_back(Act::Chunk {
                head: Box::new(head),
                columns,
            });
        }

        // phase current
        let hz = u64::from(d.phase_hz) * 3_600;
        let samples = first_at(t0, hz)..first_at(t1, hz);
        if !samples.is_empty() {
            let def = crate::defs::phase_def(d.phase_hz, d.summaries)
                .ok_or(Error::Settings("phase_hz is not 0"))?;
            let values: Vec<i16> = if self.lean {
                Vec::new()
            } else {
                phase_values(&d, samples.start, samples.end - samples.start)
            };
            // --8<-- [start:samples]
            let shape = crate::defs::phase_shape(&def);
            let mut bytes = vec![0u8; max_encoded_len(PHASE_CODEC, shape, values.len())?];
            let len = encode(PHASE_CODEC, shape, &values, &mut bytes)?;
            bytes.truncate(len);
            let grid: SeriesGrid = def.first_grid(0);
            let count = u32::try_from(samples.end - samples.start)
                .map_err(|_e| Error::Settings("a block of phase_a holds at most 2^32 samples"))?;
            let (first_tick, last_tick) =
                grid.chunk_ticks(def.period, ItemNum::new(samples.start), count)?;
            let head = ChunkHead {
                kind: BlockKind::Series,
                track_id: key("phase_a").id(),
                track_number: self.numbers.phase_a,
                first_item: ItemNum::new(samples.start),
                count,
                first_tick,
                last_tick,
                latest_end: last_tick,
                time_mode: 0,
                end_mode: EndMode::Absent,
                grid: Some(grid),
                tile: ww_tape::TileCoords::NONE,
                level: 0,
            };
            let spec = ColumnSpec {
                kind: ColumnKind::Samples,
                codec: PHASE_CODEC,
                param: 0,
                len: bytes.len(),
            };
            // --8<-- [end:samples]
            self.queue.push_back(Act::Chunk {
                head: Box::new(head),
                // a lean run's chunk holds its head only: the recorder makes its samples
                columns: if self.lean {
                    Vec::new()
                } else {
                    vec![(spec, bytes)]
                },
            });
            // --8<-- [start:push]
            if let Some(s) = &mut self.summaries {
                s.push(&values, 0)?; // a second of samples, flags 0
                // the blocks of the levels that filled, for the writer's defer_block
                self.queue
                    .extend(s.take_blocks().into_iter().map(Act::Summary));
            }
            // --8<-- [end:push]
        }
        Ok(())
    }
}

impl Iterator for Acts {
    type Item = Result<Act, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.queue.is_empty() && !(self.finished && self.block >= self.blocks) {
            if let Err(e) = self.fill() {
                return Some(Err(e));
            }
        }
        self.queue.pop_front().map(Ok)
    }
}

/// What an act did: where it was made and what it wrote.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Made {
    /// The piece it was made in.
    piece: u64,
    /// The piece's bytes before and after it.
    before: u64,
    after: u64,
    /// Where its block starts in the whole recording, the pieces before it counted: for a chunk.
    at: u64,
    /// Its block's length: for a chunk.
    len: u64,
    /// The track and first tick of its chunk; None for no chunk.
    track: Option<TrackNum>,
    first_tick: i64,
    /// The first item of its chunk.
    first_item: u64,
    /// Items in its chunk.
    items: u64,
    /// For a put of the mode: whether it wrote a record; its ticks are in `first_tick`.
    pub(crate) put: Option<bool>,
}

// --8<-- [start:options_item]
/// The chain's options for `drive`.
pub(crate) fn options(drive: &Drive) -> ChainOptions {
    // --8<-- [start:options]
    let definitions = StatePolicy {
        near: drive.near_bytes,
        copies: exact_units::Count::new(2),
    };
    ChainOptions {
        writer: WriterOptions {
            sync: SyncPolicy::default(),
            index: IndexPolicy {
                bytes: drive.checkpoint_bytes,
                interval: None,
                ..IndexPolicy::default()
            },
            layout: drive.layout.with_definitions(definitions),
            producer: PRODUCER.to_owned(),
            near: NearPolicy {
                copy_spacing: drive.copy_spacing,
                ..NearPolicy::default()
            },
            // no clock: no sync, checkpoint or cut is due by time, so the bytes follow from the acts alone
            clock: ClockRef::NONE,
            // every CRC and hash: the drive's log is kept and handed on
            integrity: ww_tape::Integrity::Full,
        },
        cut: CutPolicy {
            bytes: exact_units::Bytes::new(0),
            chunks: exact_units::Count::new(drive.piece_chunks()),
            interval: None,
        },
        tapes: PieceTapes::Each,
    }
    // --8<-- [end:options]
}
// --8<-- [end:options_item]

/// The mux's options: no tick, so that nothing is placed by time and the bytes follow from the acts alone.
fn mux_options() -> MuxOptions {
    MuxOptions {
        tick: std::time::Duration::MAX,
        ..MuxOptions::default()
    }
}

// --8<-- [start:recorder_item]
/// A chain written through a mux, one handle for each kind of act.
pub(crate) struct Recorder<T: PieceStore = MemPieces> {
    mux: TapeMux<T>,
    drive: Drive,
    /// The numbers the mux gave the drive's clocks and tracks.
    pub(crate) numbers: DriveNumbers,
    mode: StateHandle<T>,
    records: RecordsHandle<T>,
    can0: ChunkSender<T>,
    /// Made at the first chunk of `phase_a` it is given, which says where the track goes on from.
    phase_a: Option<GridHandle<T, i16>>,
    watch: ProgressWatch,
    /// The piece and its bytes after the last act.
    seen: (u64, u64),
    /// Whether each act waits until it is placed, to say where it lies: only a crash and its resume need that.
    placed: bool,
}
// --8<-- [end:recorder_item]

impl<T> Recorder<T>
where
    T: PieceStore<Error = MemError> + Clone + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
{
    /// A new chain of `drive` in `tapes`.
    fn create(drive: &Drive, tapes: &T) -> Result<Self, Error> {
        Self::create_with(drive, tapes, options(drive))
    }

    /// A new chain of `drive` in `tapes` under `options`, which differ from [options] in a policy under test.
    pub(crate) fn create_with(
        drive: &Drive,
        tapes: &T,
        options: ChainOptions,
    ) -> Result<Self, Error> {
        // --8<-- [start:create]
        let mux = TapeMux::create(
            tapes.clone(),
            OWNER,
            drive.chain,
            &[],
            options,
            mux_options(),
        )?;
        // --8<-- [end:create]
        Self::start(drive, mux)
    }

    /// The chain in `tapes` after a crash, resumed: the tracks defined again, so that they get their numbers back.
    fn resume(drive: &Drive, tapes: &T) -> Result<(Self, Resumed), Error> {
        let (mux, resumed) = TapeMux::resume(
            tapes.clone(),
            OWNER,
            drive.chain,
            options(drive),
            mux_options(),
        )?;
        Ok((Self::start(drive, mux)?, resumed))
    }

    fn start(drive: &Drive, mux: TapeMux<T>) -> Result<Self, Error> {
        // --8<-- [start:handles]
        let numbers = define_on_mux(
            &mux,
            drive.phase_hz,
            drive.summaries,
            drive.near_bytes.get(),
        )?;
        let mode = mux.state(numbers.mode)?;
        let records = mux.records();
        let can0 = mux.chunks(numbers.can0)?;
        // --8<-- [end:handles]
        let watch = mux.watch();
        mux.flush()?;
        let p = watch.get();
        Ok(Self {
            mux,
            drive: *drive,
            numbers,
            mode,
            records,
            can0,
            phase_a: None,
            watch,
            seen: (p.piece, p.piece_bytes),
            placed: true,
        })
    }

    /// The recorder without positions: an act is handed over and the next one made while the writer places it.
    fn unplaced(mut self) -> Self {
        self.placed = false;
        self
    }

    /// The handle of `phase_a`, started at the item and tick of `head` when it has none yet.
    fn phase_handle(&mut self, head: &ChunkHead) -> Result<&mut GridHandle<T, i16>, Error> {
        if self.phase_a.is_none() {
            // --8<-- [start:series]
            let options = GridOptions {
                chunk_steps: u32::MAX,
                chunk_bytes: exact_units::Bytes::new(u32::MAX),
                latency: None, // a chunk is completed after each block of the run, not by time
                codec: PHASE_CODEC,
                summaries: self.drive.summaries,
                first_item: head.first_item,
                first_tick: head.first_tick,
            };
            let mut phase_a = self.mux.grid::<i16>(self.numbers.phase_a, options)?;
            if head.first_item.get() > 0 {
                // a resumed track goes on on the grid the run started with, from sample 0
                let def = crate::defs::phase_def(self.drive.phase_hz, self.drive.summaries)
                    .ok_or(Error::Settings("phase_hz is not 0"))?;
                phase_a.regrid(def.first_grid(0))?;
            }
            self.phase_a = Some(phase_a);
            // --8<-- [end:series]
        }
        self.phase_a
            .as_mut()
            .ok_or(Error::Settings("the handle of phase_a"))
    }

    /// Hand `act` to its handle and wait until it is placed.
    pub(crate) fn apply(&mut self, act: &Act) -> Result<Made, Error> {
        let mut made = Made {
            piece: self.seen.0,
            before: self.seen.1,
            after: self.seen.1,
            at: 0,
            len: 0,
            track: None,
            first_tick: 0,
            first_item: 0,
            items: 0,
            put: None,
        };
        // --8<-- [start:apply]
        match act {
            Act::Put { ticks, mode } => {
                self.mode.put(*ticks, mode.item())?;
                made.first_tick = *ticks;
                made.put = Some(false);
            }
            Act::Records(records) => {
                // the mux numbers the block as the writer's next
                let id = BlockId::new(SessionTag::NONE, 0);
                let block = records_block(id, self.numbers.board_clock, records)?;
                self.records.send(block)?;
            }
            Act::Chunk { head, columns } if head.track_number == self.numbers.can0 => {
                // frames are encoded as the page "Columns" says: the sender takes the chunk as it is
                self.can0.send(**head, columns.clone())?;
            }
            Act::Chunk { head, columns } => {
                let values = chunk_values(&self.drive, head, columns)?;
                let phase_a = self.phase_handle(head)?;
                phase_a.push(&values)?;
                phase_a.complete()?; // a chunk a block of the run
            }
            // the mux builds the summaries of `phase_a` itself
            Act::Summary(_) => return Ok(made),
        }
        // --8<-- [end:apply]
        if let Act::Chunk { head, .. } = act {
            made.track = Some(head.track_number);
            made.first_tick = head.first_tick;
            made.first_item = head.first_item.get();
            made.items = u64::from(head.count);
        }
        if !self.placed {
            return Ok(made);
        }
        self.mux.flush()?;
        let p = self.watch.get();
        if p.piece == self.seen.0 {
            made.before = self.seen.1;
        }
        made.piece = p.piece;
        made.after = p.piece_bytes;
        if made.put.is_some() {
            made.put = Some(made.after != made.before || p.piece != self.seen.0);
        }
        self.seen = (p.piece, p.piece_bytes);
        Ok(made)
    }

    /// The piece being written and its bytes, after the last act.
    pub(crate) fn position(&self) -> (u64, u64) {
        self.seen
    }

    /// Close the chain: the pieces it holds, closed.
    pub(crate) fn finish(mut self) -> Result<Vec<ChainPiece<T::Key>>, Error> {
        drop(self.phase_a.take());
        Ok(self.mux.close()?)
    }
}

// --8<-- [start:records_block_item]
/// A records block numbered `id` of `records` on the board's clock `board`, (track, ticks, item) in time order.
pub fn records_block(
    id: BlockId,
    board: ClockNum,
    records: &[(TrackNum, i64, Vec<u8>)],
) -> Result<Vec<u8>, Error> {
    let len = records
        .iter()
        .map(|(_, _, item)| item.len() + RECORD_OVERHEAD_MAX)
        .sum::<usize>()
        + 256;
    let mut buf = vec![0u8; len];
    // --8<-- [start:records]
    let mut block = BlockWriter::new(&mut buf, BlockKind::Records, id, &[board])?;
    for (track, ticks, item) in records {
        block.push(*track, ClockTime::new(board, *ticks), &[item])?;
    }
    let len = block.finish(); // the block's header, records and CRC: its bytes in buf
    // --8<-- [end:records]
    buf.truncate(len);
    Ok(buf)
}
// --8<-- [end:records_block_item]

/// Whether an act made as `made` is to be given again to a chain resumed in piece `piece` with `kept` bytes of that
/// piece kept: a block it doesn't hold, a put made after the last block it kept. The mux builds summaries itself,
/// from the chunks the piece kept.
fn lost(act: &Act, made: &Made, piece: u64, kept: u64) -> bool {
    match act {
        Act::Summary(_) => false,
        // a put whose last block is kept isn't given again: its records changed what the writer remembers
        Act::Put { .. } => {
            made.piece > piece
                || (made.piece == piece
                    && (made.after > kept || (made.after == kept && made.before == made.after)))
        }
        _ => made.piece > piece || (made.piece == piece && made.after > kept),
    }
}

/// What a recording made without positions refuses.
const UNPLACED: &str = "a recording made without positions";

/// A recording of the drive: its pieces in chain order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    /// What was recorded.
    pub drive: Drive,
    /// The bytes of each piece, in chain order.
    pub pieces: Vec<Vec<u8>>,
    /// The chain's closed pieces: their tapes and how each ended.
    pub closed: Vec<ChainPiece<u64>>,
    /// The numbers the drive's clocks and tracks were given.
    pub numbers: DriveNumbers,
    /// What each act did, in order.
    pub(crate) made: Vec<Made>,
    /// Whether `made` says where each act was written: not for [Drive::record_unplaced].
    placed: bool,
}

impl Recording {
    /// Bytes of the recording, every piece counted.
    pub fn len(&self) -> u64 {
        self.closed.iter().map(|p| p.piece.len).sum()
    }

    /// Whether the recording holds no byte.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The pieces back to back, as one file of the chain holds them.
    pub fn concat(&self) -> Vec<u8> {
        self.pieces.concat()
    }

    /// The chunks of track `track`: where each starts in the recording, its length and items.
    fn chunks(&self, track: TrackNum) -> impl Iterator<Item = &Made> + '_ {
        self.made
            .iter()
            .filter(move |m| m.track == Some(track) && m.len > 0)
    }

    /// Bytes of the chunks of `track` and the items in them.
    pub fn chunk_bytes(&self, track: TrackNum) -> (u64, u64) {
        self.chunks(track)
            .fold((0, 0), |(bytes, items), m| (bytes + m.len, items + m.items))
    }

    /// The byte the story's crash happens at: the middle of the first chunk of `can0` at or after minute 37.
    pub fn story_crash_byte(&self) -> Result<u64, Error> {
        let at = i64::from(CRASH_MINUTE) * 60_000 * US_PER_MS;
        self.chunks(self.numbers.can0)
            .find(|m| m.first_tick >= at)
            .map(|m| m.at + m.len / 2)
            .ok_or(Error::Settings("the run ends before minute 37"))
    }

    /// The byte in the middle of the first chunk of `track` at or after `ticks`, counted in the whole recording.
    pub(crate) fn chunk_at(&self, track: TrackNum, ticks: i64) -> Option<u64> {
        self.chunks(track)
            .find(|m| m.first_tick >= ticks)
            .map(|m| m.at + m.len / 2)
    }

    /// The puts of the mode made in piece `piece`, and the ticks of those that wrote a record: the first record of a
    /// mode, its copy, or the mode again once its record lies more than `near` behind.
    pub fn mode_puts(&self, piece: u64) -> (usize, Vec<i64>) {
        debug_assert!(self.placed, "{UNPLACED}");
        let puts = self
            .made
            .iter()
            .filter(|m| m.piece == piece && m.put.is_some());
        let wrote = puts
            .clone()
            .filter(|m| m.put == Some(true))
            .map(|m| m.first_tick)
            .collect();
        (puts.count(), wrote)
    }

    /// This recording cut off at `byte` ([Drive::crash]).
    pub fn crash(&self, byte: u64) -> Result<Crashed, Error> {
        if !self.placed {
            return Err(Error::Settings(UNPLACED));
        }
        let (piece, offset) = self.locate(byte).ok_or(Error::Settings(CRASH_PAST_END))?;
        let at = usize::try_from(piece).map_err(|_e| Error::Settings("a piece number"))?;
        let torn = self
            .pieces
            .get(at)
            .ok_or(Error::Settings("the piece the crash tore"))?;
        let done = self
            .pieces
            .get(..at)
            .ok_or(Error::Settings("the pieces before the crash"))?;
        let cut = usize::try_from(offset).map_err(|_e| Error::Settings("a crash offset"))?;
        Ok(Crashed {
            drive: self.drive,
            tapes: plant(done, torn, cut)?,
            made: self.made.clone(),
            crash: Crash {
                byte,
                piece,
                offset,
            },
        })
    }

    /// The piece and the offset in it of byte `byte` of the recording.
    pub fn locate(&self, byte: u64) -> Option<(u64, u64)> {
        let mut start = 0;
        for (seq, p) in self.closed.iter().enumerate() {
            if byte < start + p.piece.len {
                return Some((u64::try_from(seq).ok()?, byte - start));
            }
            start += p.piece.len;
        }
        None
    }
}

/// Where a crash happened.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Crash {
    /// Bytes of the recording before the crash, every piece counted.
    pub byte: u64,
    /// The piece it tore.
    pub piece: u64,
    /// Bytes of that piece on disk.
    pub offset: u64,
}

/// A recording a crash cut off: the pieces closed before it and the unclosed piece it tore.
pub struct Crashed {
    drive: Drive,
    tapes: MemPieces,
    made: Vec<Made>,
    /// Where the crash happened.
    pub crash: Crash,
}

/// A crashed recording resumed and given its lost acts again.
pub struct Resume {
    /// What the resume of the torn piece found.
    pub resumed: Resumed,
    /// Acts made before the crash and given again: the blocks and puts the crash lost.
    pub lost: usize,
    /// Acts given after the resume: those lost and the rest of the run.
    pub replayed: usize,
    /// The recording after the resume, closed.
    pub recording: Recording,
}

impl Crashed {
    /// The bytes of each piece on disk after the crash: those the chain closed, then the torn one.
    pub fn pieces(&self) -> Result<Vec<Vec<u8>>, Error> {
        (0..=self.crash.piece)
            .map(|seq| {
                self.tapes
                    .get(&seq)?
                    .ok_or(Error::Settings("a piece before the crash"))?
                    .to_vec()
                    .map_err(Error::from)
            })
            .collect()
    }

    /// Resume the chain and give it every act the crash lost, then close it.
    pub fn resume(self) -> Result<Resume, Error> {
        let (mut r, resumed) = Recorder::resume(&self.drive, &self.tapes)?;
        let (piece, kept) = (self.crash.piece, resumed.kept_bytes);
        let mut made = Vec::new();
        let (mut lost_acts, mut replayed) = (0, 0);
        for (i, act) in Acts::lean(self.drive)?.enumerate() {
            let act = act?;
            match self.made.get(i) {
                Some(m) if !lost(&act, m, piece, kept) => made.push(*m),
                was => {
                    made.push(r.apply(&act)?);
                    replayed += 1;
                    // the acts the run made before the crash; those after it were never made
                    lost_acts += usize::from(was.is_some_and(|m| {
                        m.piece < piece || (m.piece == piece && m.before < self.crash.offset)
                    }));
                }
            }
        }
        Ok(Resume {
            resumed,
            lost: lost_acts,
            replayed,
            recording: close(self.drive, r, &self.tapes, made, true)?,
        })
    }
}

/// The definitions `drive` records with, and the numbers they give.
pub(crate) fn drive_definitions(drive: &Drive) -> Result<(Definitions, DriveNumbers), Error> {
    definitions(drive.phase_hz, drive.summaries, drive.near_bytes.get())
}

/// Close the chain of `r`, read its pieces from `tapes` and place each chunk `made` lists in them.
fn close(
    drive: Drive,
    r: Recorder,
    tapes: &MemPieces,
    mut made: Vec<Made>,
    placed: bool,
) -> Result<Recording, Error> {
    let numbers = r.numbers;
    let closed = r.finish()?;
    let pieces: Vec<Vec<u8>> = closed
        .iter()
        .map(|p| {
            tapes
                .get(&p.tape)?
                .ok_or(Error::Settings("a closed piece"))?
                .to_vec()
                .map_err(Error::from)
        })
        .collect::<Result<_, _>>()?;
    place_chunks(&mut made, &pieces)?;
    settle(&mut made, &pieces)?;
    Ok(Recording {
        drive,
        pieces,
        closed,
        numbers,
        made,
        placed,
    })
}

/// Fill in where each chunk of `made` lies in the recording, from the pieces' own indexes.
fn place_chunks(made: &mut [Made], pieces: &[Vec<u8>]) -> Result<(), Error> {
    let mut start = 0u64;
    for piece in pieces {
        let reader = TapeReader::open(&piece[..])?;
        for e in reader.entries(0)? {
            if !matches!(e.kind, BlockKind::Events | BlockKind::Series) {
                continue;
            }
            let item = e.first_item.get();
            let slot = made
                .iter_mut()
                .find(|m| m.track == e.track() && m.first_item == item && m.items > 0);
            if let Some(m) = slot {
                m.at = start + e.offset;
                m.len = u64::from(e.len);
            }
        }
        start += u64::try_from(piece.len()).map_err(|_e| Error::Settings("a piece's length"))?;
    }
    Ok(())
}

/// Move the end of each act before the checkpoints and summaries the writer placed right after its blocks, with the
/// puts a summary needs: they are no block of the act, so a crash that tears only them keeps the act: the resume
/// writes the summaries it lost.
fn settle(made: &mut [Made], pieces: &[Vec<u8>]) -> Result<(), Error> {
    for (seq, piece) in (0u64..).zip(pieces) {
        let mut checkpoints = BTreeMap::new();
        let mut walk = FileBlocks::with_recovery(&piece[..], Recovery::Prefix);
        // where the blocks of puts that belong with the next block start, while they go on
        let mut puts: Option<u64> = None;
        while let Some(block) = walk.next_block() {
            let (kind, len, with_next) = {
                let block = block?;
                let with_next = block.is_repeat() && block.flags & ww_tape::FLAG_WITH_NEXT != 0;
                (
                    block.kind,
                    u64::try_from(block.bytes().len()).unwrap_or(0),
                    with_next,
                )
            };
            let start = walk.offset();
            if kind == BlockKind::Index || kind.is_summary() {
                let from = if kind.is_summary() {
                    puts.unwrap_or(start)
                } else {
                    start
                };
                checkpoints.insert(start.saturating_add(len), from);
            }
            puts = if with_next {
                puts.or(Some(start))
            } else {
                None
            };
        }
        let before_checkpoints = |mut at: u64| {
            while let Some(&start) = checkpoints.get(&at) {
                at = start;
            }
            at
        };
        for m in made.iter_mut().filter(|m| m.piece == seq) {
            m.before = before_checkpoints(m.before);
            m.after = before_checkpoints(m.after);
        }
    }
    Ok(())
}

/// A store holding the pieces `done`, closed, and the first `offset` bytes of `torn` as the unclosed piece after
/// them: a chain cut off at that byte.
fn plant(done: &[Vec<u8>], torn: &[u8], offset: usize) -> Result<MemPieces, Error> {
    let tapes = MemPieces::new();
    for (seq, bytes) in (0u64..).zip(done) {
        let mut sink = tapes.create(&seq)?;
        sink.write_all(bytes)?;
        sink.close()?;
    }
    let mut sink =
        tapes.create(&u64::try_from(done.len()).map_err(|_e| Error::Settings("pieces"))?)?;
    sink.write_all(
        torn.get(..offset)
            .ok_or(Error::Settings("the crash offset"))?,
    )?;
    Ok(tapes)
}

impl Drive {
    // --8<-- [start:drive_acts_item]
    /// The acts of the run in order: for each block of [Drive::block_ms] the puts of the mode, a records block, a
    /// chunk of frames, a chunk of samples and the summary blocks those samples finish; the last summaries at the end.
    pub fn acts(&self) -> Result<impl Iterator<Item = Result<Act, Error>> + use<>, Error> {
        Acts::new(*self)
    }
    // --8<-- [end:drive_acts_item]

    // --8<-- [start:drive_definitions_item]
    /// The definitions the run records with: the two clocks, the map between them and the five tracks; and the
    /// numbers they give the clocks and tracks.
    pub fn definitions(&self) -> Result<(Definitions, DriveNumbers), Error> {
        drive_definitions(self)
    }

    /// The numbers the run's definitions give its clocks and tracks.
    pub fn numbers(&self) -> Result<DriveNumbers, Error> {
        Ok(drive_definitions(self)?.1)
    }
    // --8<-- [end:drive_definitions_item]

    /// Record the run whole and close the chain.
    pub fn record(&self) -> Result<Recording, Error> {
        self.record_acts(&Acts::lean(*self)?.collect::<Result<Vec<_>, _>>()?)
    }

    /// Record the run whole and close the chain, without waiting for the writer after each act: the same bytes as
    /// [Drive::record], made while the producer and the writer thread overlap. The recording cannot [Recording::crash]:
    /// it does not know where each act was written.
    pub fn record_unplaced(&self) -> Result<Recording, Error> {
        let tapes = MemPieces::new();
        let mut r = Recorder::create(self, &tapes)?.unplaced();
        let mut made = Vec::new();
        for act in Acts::lean(*self)? {
            made.push(r.apply(&act?)?);
        }
        close(*self, r, &tapes, made, false)
    }

    /// Record `acts`, made by [Drive::acts] of a run that differs from this one in its byte policies only: the same
    /// recording as [Drive::record], without making the acts again.
    pub(crate) fn record_acts(&self, acts: &[Act]) -> Result<Recording, Error> {
        let tapes = MemPieces::new();
        let mut r = Recorder::create(self, &tapes)?;
        // the mux builds the summaries itself, as the acts of a lean run say
        let made = acts
            .iter()
            .filter(|act| !matches!(act, Act::Summary(_)))
            .map(|act| r.apply(act))
            .collect::<Result<Vec<_>, _>>()?;
        close(*self, r, &tapes, made, true)
    }

    /// Record the run, then cut its bytes at `byte`: the recording ends after `byte` bytes, its last piece unclosed.
    ///
    /// The crash may fall anywhere: in a block of data, in a checkpoint, or in the end of a piece while a cut or the
    /// close writes its final index, close and tail; the pieces after the torn one are gone. A byte at or past the end
    /// of the recording is refused: [CRASH_PAST_END].
    pub fn crash(&self, byte: u64) -> Result<Crashed, Error> {
        self.record()?.crash(byte)
    }

    /// The story the book tells: the run recorded whole, the crash at minute 37 and its resume.
    pub fn story(&self) -> Result<Story, Error> {
        self.record()?.story()
    }
}

impl Recording {
    /// The story of this recording: the crash at minute 37 ([Self::story_crash_byte]), the resume, and what each
    /// cost.
    pub fn story(&self) -> Result<Story, Error> {
        let crashed = self.crash(self.story_crash_byte()?)?;
        let crash = crashed.crash;
        let resume = crashed.resume()?;
        let (frame_bytes, frames) = self.chunk_bytes(self.numbers.can0);
        let (sample_bytes, samples) = self.chunk_bytes(self.numbers.phase_a);
        let torn = self
            .chunks(self.numbers.can0)
            .find(|m| m.at <= crash.byte && crash.byte < m.at + m.len)
            .ok_or(Error::Settings("the crash tears a chunk of frames"))?;
        Ok(Story {
            drive: self.drive,
            pieces: self.closed.iter().map(|p| p.piece.len).collect(),
            frames,
            frame_bytes,
            samples,
            sample_bytes,
            crash,
            torn_from_us: torn.first_tick,
            kept_bytes: resume.resumed.kept_bytes,
            lost_acts: resume.lost,
            same_bytes: resume.recording.pieces == self.pieces,
            fault_minute: FAULT_MINUTE,
            emergency_frame: emergency_frame(1_000_000 / u64::from(self.drive.can_hz)),
        })
    }
}

/// The numbers of the story, as the code makes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Story {
    pub drive: Drive,
    /// Bytes of each piece of the chain.
    pub pieces: Vec<u64>,
    /// Frames on `can0` and the bytes of their chunks.
    pub frames: u64,
    pub frame_bytes: u64,
    /// Samples of `phase_a` and the bytes of their chunks.
    pub samples: u64,
    pub sample_bytes: u64,
    /// Where the host lost power.
    pub crash: Crash,
    /// The first frame time of the chunk the crash tore, µs since boot.
    pub torn_from_us: i64,
    /// Bytes of the torn piece the resume kept.
    pub kept_bytes: u64,
    /// Acts made before the crash that the recorder gave again after the resume.
    pub lost_acts: usize,
    /// The resumed chain holds the bytes of the one written without the crash.
    pub same_bytes: bool,
    /// The minute the drive faults.
    pub fault_minute: u32,
    /// The number of the emergency frame on `can0`.
    pub emergency_frame: u64,
}
