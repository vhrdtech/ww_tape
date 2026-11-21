//! The recorder: the acts of the run block by block, written into a chain of pieces in memory, a crash at a byte
//! and the resume.

use std::collections::VecDeque;
use ww_tape::{ClockNum, TrackNum};

use ww_tape::chunk::ChunkHead;
use ww_tape::index::IndexPolicy;
use ww_tape::payloads::EncodedPayloads;
use ww_tape::samples::{encode, max_encoded_len};
use ww_tape::times::EncodedTimes;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ChainOptions, ChainPiece, ClockRef, ClockTime, Codec,
    ColumnKind, ColumnSpec, CutPolicy, Definitions, EndMode, MemError, MemTapes, PieceChain,
    PieceTapes, Resumed, SeriesGrid, StatePolicy, SyncPolicy, TapeSink, Tapes, WriterOptions,
};
use ww_tape_summary::{SeriesStart, SeriesSummaries};

use crate::defs::{BOARD_CLOCK, CAN0, LOG, MODE, PHASE_A, TEMP, definitions, key};
use crate::signals::{
    CRASH_MINUTE, FAULT_MINUTE, Mode, US_PER_HOUR, US_PER_MS, emergency_frame, frame, log_line,
    phase_ma, temp_centi,
};
use crate::{Drive, Error, OWNER, PRODUCER};

/// The codec of `phase_a`'s samples: differences, bit-packed in groups.
const PHASE_CODEC: Codec = Codec::DELTA;
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
    Records(Vec<(u32, i64, Vec<u8>)>),
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
    /// The next block of the run
    block: u64,
    blocks: u64,
    summaries: Option<SeriesSummaries<i16>>,
    queue: VecDeque<Act>,
    finished: bool,
}

/// The first item of a track of `per_hour` items, item `i` at `i * US_PER_HOUR / per_hour` µs, at or after `us`.
fn first_at(us: i64, per_hour: u64) -> u64 {
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
    fn new(drive: Drive) -> Result<Self, Error> {
        if drive.phase_hz == 0 || 1_000_000 % drive.phase_hz != 0 {
            return Err(Error::Settings("phase_hz divides 1 000 000"));
        }
        if drive.can_hz == 0 || 1_000_000 % drive.can_hz != 0 {
            return Err(Error::Settings("can_hz divides 1 000 000"));
        }
        if drive.block_ms == 0 || 60_000 % drive.block_ms != 0 {
            return Err(Error::Settings("block_ms divides a minute"));
        }
        let run_ms = u64::from(drive.minutes) * 60_000;
        let blocks = run_ms.div_ceil(u64::from(drive.block_ms));
        let summaries = if drive.summaries {
            let def = crate::defs::phase_def(drive.phase_hz, true)
                .ok_or(Error::Settings("phase_hz is not 0"))?;
            // --8<-- [start:summaries]
            let start = SeriesStart {
                track_id: key("phase_a"),
                track_number: TrackNum::new(PHASE_A),
                shape: crate::defs::phase_shape(&def),
                first_item: 0,
                grid: def.first_grid(0),
                period: def.period,
            };
            Some(SeriesSummaries::new(start, crate::defs::phase_summaries())?)
            // --8<-- [end:summaries]
        } else {
            None
        };
        Ok(Self {
            drive,
            block: 0,
            blocks,
            summaries,
            queue: VecDeque::new(),
            finished: false,
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
                records.push((LOG, us, log_line(n, us / US_PER_MS).into_bytes()));
            }
        }
        if temp > 0 {
            for n in first_at(t0, temp)..first_at(t1, temp) {
                let us = item_us(n, temp);
                records.push((TEMP, us, temp_centi(us / US_PER_MS).to_le_bytes().to_vec()));
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
            let head = times
                .head
                .chunk_head(key("can0"), TrackNum::new(CAN0), frames.start);
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
            let values: Vec<i16> = samples
                .clone()
                .map(|n| phase_ma(item_us(n, hz), n, d.phase_hz))
                .collect();
            // --8<-- [start:samples]
            let shape = crate::defs::phase_shape(&def);
            let mut bytes = vec![0u8; max_encoded_len(PHASE_CODEC, shape, values.len())?];
            let len = encode(PHASE_CODEC, shape, &values, &mut bytes)?;
            bytes.truncate(len);
            let grid: SeriesGrid = def.first_grid(0);
            let count = u32::try_from(values.len())
                .map_err(|_e| Error::Settings("a block of phase_a holds at most 2^32 samples"))?;
            let (first_tick, last_tick) = grid.chunk_ticks(def.period, samples.start, count)?;
            let head = ChunkHead {
                kind: BlockKind::Series,
                track_id: key("phase_a"),
                track_number: TrackNum::new(PHASE_A),
                first_item: samples.start,
                count,
                first_tick,
                last_tick,
                latest_end: last_tick,
                time_mode: 0,
                end_mode: EndMode::Absent,
                grid: Some(grid),
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
                columns: vec![(spec, bytes)],
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
    /// The seq of the writer's last block after it.
    seq: u64,
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
        near: exact_units::Bytes::new(drive.near_bytes),
        copies: exact_units::Count::new(1),
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
        },
        cut: CutPolicy {
            bytes: 0,
            chunks: drive.piece_chunks(),
            interval: None,
        },
        tapes: PieceTapes::Each,
    }
    // --8<-- [end:options]
}
// --8<-- [end:options_item]

// --8<-- [start:recorder_item]
/// A chain and what it was written with.
pub(crate) struct Recorder<T: Tapes<Error = MemError> = MemTapes> {
    pub(crate) chain: PieceChain<T>,
    pub(crate) defs: Definitions,
}
// --8<-- [end:recorder_item]

impl<T: Tapes<Error = MemError>> Recorder<T> {
    /// Bytes of the pieces the chain has closed.
    fn closed_bytes(&self) -> u64 {
        self.chain.closed().iter().map(|p| p.piece.len).sum()
    }

    /// Bytes of the whole recording so far.
    pub(crate) fn bytes(&mut self) -> Result<u64, Error> {
        let open = self.chain.writer()?.bytes();
        Ok(self.closed_bytes() + open)
    }

    /// Hand `act` to the chain.
    pub(crate) fn apply(&mut self, act: &Act) -> Result<Made, Error> {
        // --8<-- [start:cut]
        if matches!(act, Act::Chunk { .. }) && self.chain.cut_due() {
            // the cut comes first, so that the chunk is made in the piece it lands in
            self.chain.cut_now(&self.defs)?;
        }
        // --8<-- [end:cut]
        let piece = self.chain.seq();
        let before = self.chain.writer()?.bytes();
        let mut made = Made {
            piece,
            seq: 0,
            before,
            after: before,
            at: 0,
            len: 0,
            track: None,
            first_tick: 0,
            items: 0,
            put: None,
        };
        match act {
            Act::Put { ticks, mode } => {
                made.put =
                    Some(
                        self.chain
                            .put(&self.defs, TrackNum::new(MODE), *ticks, mode.item())?,
                    );
                made.first_tick = *ticks;
            }
            Act::Records(records) => {
                let block = records_block(self.chain.writer()?.next_id(), records)?;
                self.chain.write_block(&self.defs, &block)?;
            }
            Act::Chunk { head, columns } => {
                let columns: Vec<(ColumnSpec, &[u8])> =
                    columns.iter().map(|(s, b)| (*s, b.as_slice())).collect();
                let placed = self.chain.write_chunk(&self.defs, head, &columns)?;
                made.piece = placed.seq;
                made.at = self.closed_bytes() + placed.offset;
                made.len = u64::from(placed.len);
                made.track = Some(head.track_number);
                made.first_tick = head.first_tick;
                made.items = u64::from(head.count);
            }
            Act::Summary(block) => {
                self.chain.defer_block(&self.defs, block.clone())?;
            }
        }
        let w = self.chain.writer()?;
        made.after = w.bytes();
        made.seq = w.next_seq().wrapping_sub(1);
        Ok(made)
    }
}

// --8<-- [start:records_block_item]
/// A records block numbered `id` of `records`, (track, ticks of the board's clock, item) in time order.
pub fn records_block(id: BlockId, records: &[(u32, i64, Vec<u8>)]) -> Result<Vec<u8>, Error> {
    let len = records
        .iter()
        .map(|(_, _, item)| item.len() + RECORD_OVERHEAD_MAX)
        .sum::<usize>()
        + 256;
    let mut buf = vec![0u8; len];
    // --8<-- [start:records]
    let mut block = BlockWriter::new(
        &mut buf,
        BlockKind::Records,
        id,
        &[ClockNum::new(BOARD_CLOCK)],
    )?;
    for (track, ticks, item) in records {
        block.push(
            TrackNum::new(*track),
            ClockTime::new(ClockNum::new(BOARD_CLOCK), *ticks),
            &[item],
        )?;
    }
    let len = block.finish(); // the block's header, records and CRC: its bytes in buf
    // --8<-- [end:records]
    buf.truncate(len);
    Ok(buf)
}
// --8<-- [end:records_block_item]

/// Whether act `i` is to be given again to a chain resumed in piece `piece` with next seq `next`, `kept` bytes of
/// that piece kept: a block it doesn't hold, a put made after the last block it kept.
fn lost(act: &Act, made: &Made, piece: u64, next: u64, kept: u64) -> bool {
    match act {
        // a put whose last block is kept isn't given again: its records changed what the writer remembers
        Act::Put { .. } => {
            made.piece > piece
                || (made.piece == piece
                    && (made.after > kept || (made.after == kept && made.before == made.after)))
        }
        _ => made.piece > piece || (made.piece == piece && made.seq >= next),
    }
}

/// A recording of the drive: its pieces in chain order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    /// What was recorded.
    pub drive: Drive,
    /// The bytes of each piece, in chain order.
    pub pieces: Vec<Vec<u8>>,
    /// The chain's closed pieces: their tapes and how each ended.
    pub closed: Vec<ChainPiece<u64>>,
    /// What each act did, in order.
    pub(crate) made: Vec<Made>,
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
        self.chunks(TrackNum::new(CAN0))
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
    tapes: MemTapes,
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
        let (chain, resumed) = PieceChain::resume_on(
            self.tapes.clone(),
            OWNER,
            self.drive.chain,
            options(&self.drive),
        )?;
        let mut r = Recorder {
            chain,
            defs: drive_definitions(&self.drive)?,
        };
        let piece = r.chain.seq();
        let next = r.chain.writer()?.next_seq();
        let mut made = Vec::new();
        let (mut lost_acts, mut replayed) = (0, 0);
        for (i, act) in Acts::new(self.drive)?.enumerate() {
            let act = act?;
            match self.made.get(i) {
                Some(m) if !lost(&act, m, piece, next, resumed.kept_bytes) => made.push(*m),
                was => {
                    made.push(r.apply(&act)?);
                    replayed += 1;
                    lost_acts += usize::from(was.is_some());
                }
            }
        }
        Ok(Resume {
            resumed,
            lost: lost_acts,
            replayed,
            recording: close(self.drive, r, &self.tapes, made)?,
        })
    }
}

/// The definitions `drive` records with.
pub(crate) fn drive_definitions(drive: &Drive) -> Result<Definitions, Error> {
    definitions(drive.phase_hz, drive.summaries, drive.near_bytes)
}

/// Close the chain of `r` and read its pieces from `tapes`.
fn close(drive: Drive, r: Recorder, tapes: &MemTapes, made: Vec<Made>) -> Result<Recording, Error> {
    let closed = r.chain.close()?;
    let pieces = closed
        .iter()
        .map(|p| {
            tapes
                .get(&p.tape)?
                .ok_or(Error::Settings("a closed piece"))?
                .to_vec()
                .map_err(Error::from)
        })
        .collect::<Result<_, _>>()?;
    Ok(Recording {
        drive,
        pieces,
        closed,
        made,
    })
}

impl Drive {
    fn start(&self, tapes: &MemTapes) -> Result<Recorder, Error> {
        Ok(Recorder {
            chain: PieceChain::create_on(tapes.clone(), OWNER, self.chain, &[], options(self))?,
            defs: drive_definitions(self)?,
        })
    }

    // --8<-- [start:drive_acts_item]
    /// The acts of the run in order: for each block of [Drive::block_ms] the puts of the mode, a records block, a
    /// chunk of frames, a chunk of samples and the summary blocks those samples finish; the last summaries at the end.
    pub fn acts(&self) -> Result<impl Iterator<Item = Result<Act, Error>> + use<>, Error> {
        Acts::new(*self)
    }
    // --8<-- [end:drive_acts_item]

    // --8<-- [start:drive_definitions_item]
    /// The definitions the run records with: the two clocks, the map between them and the five tracks.
    pub fn definitions(&self) -> Result<Definitions, Error> {
        drive_definitions(self)
    }
    // --8<-- [end:drive_definitions_item]

    /// Record the run whole and close the chain.
    pub fn record(&self) -> Result<Recording, Error> {
        let tapes = MemTapes::new();
        let mut r = self.start(&tapes)?;
        let made = Acts::new(*self)?
            .map(|act| r.apply(&act?))
            .collect::<Result<Vec<_>, _>>()?;
        close(*self, r, &tapes, made)
    }

    /// Record `acts`, made by [Drive::acts] of a run that differs from this one in its byte policies only: the same
    /// recording as [Drive::record], without making the acts again.
    pub(crate) fn record_acts(&self, acts: &[Act]) -> Result<Recording, Error> {
        let tapes = MemTapes::new();
        let mut r = self.start(&tapes)?;
        let made = acts
            .iter()
            .map(|act| r.apply(act))
            .collect::<Result<Vec<_>, _>>()?;
        close(*self, r, &tapes, made)
    }

    /// Record the run until the recording holds more than `byte` bytes, then crash: the recording ends after `byte`
    /// bytes, its last piece unclosed.
    ///
    /// The crash may fall anywhere: in a block of data, in a checkpoint, or in the end of a piece while a cut or the
    /// close writes its final index, close and tail; the pieces after the torn one were never made. A byte at or past
    /// the end of the recording is refused: [CRASH_PAST_END].
    pub fn crash(&self, byte: u64) -> Result<Crashed, Error> {
        let tapes = MemTapes::new();
        let mut r = self.start(&tapes)?;
        let mut made = Vec::new();
        let mut acts = Acts::new(*self)?;
        let stopped = loop {
            let Some(act) = acts.next() else { break false };
            made.push(r.apply(&act?)?);
            if r.bytes()? > byte {
                break true;
            }
        };
        if stopped {
            // the power fails: the writer stops where it is
            drop(r);
        } else {
            r.chain.close()?;
        }
        // the piece the crash tore ends where the bytes on disk end; those after it were never made
        let mut start = 0;
        let mut torn = None;
        for seq in tapes.keys()? {
            let len = tapes
                .get(&seq)?
                .map_or(Ok(0), |t| t.to_vec().map(|b| b.len() as u64))?;
            match torn {
                None if byte < start + len => torn = Some((seq, byte - start)),
                None => start += len,
                Some(_) => {
                    // a tape still being written is not removed: marked closed first
                    if let Some(mut sink) = tapes.reopen(&seq)? {
                        sink.close()?;
                    }
                    tapes.remove(&seq)?;
                }
            }
        }
        let (piece, offset) = torn.ok_or(Error::Settings(CRASH_PAST_END))?;
        let mut sink = tapes
            .reopen(&piece)?
            .ok_or(Error::Settings("the piece the crash tore"))?;
        sink.truncate(offset)?;
        drop(sink);
        Ok(Crashed {
            drive: *self,
            tapes,
            made,
            crash: Crash {
                byte,
                piece,
                offset,
            },
        })
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
        let crashed = self.drive.crash(self.story_crash_byte()?)?;
        let crash = crashed.crash;
        let resume = crashed.resume()?;
        let (frame_bytes, frames) = self.chunk_bytes(TrackNum::new(CAN0));
        let (sample_bytes, samples) = self.chunk_bytes(TrackNum::new(PHASE_A));
        let torn = self
            .chunks(TrackNum::new(CAN0))
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
