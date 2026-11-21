//! The RF capture: complex IQ samples from a software-defined radio's front end, the book's case for high rates.
//!
//! The front end's analog-to-digital converter gives 12 bits per component in 16-bit words, so a sample is a
//! `ComplexI16` of values within ±2047. The band holds three steady carriers, noise of a few steps and five bursts of
//! 2 ms a minute, each a pure function of the sample's number ([Signal::fill]), so any part of the capture is made alone and
//! every capture is the same. The frequencies are fractions of the rate, so a capture at a lower rate ([Rf::scaled])
//! holds the same signal, sample for sample.
//!
//! [Rf::record_on] writes the capture as one piece into any sink: one track `iq`, a series chunk per
//! [Rf::chunk_samples], and summary nodes of 1024 samples with the mean and the largest power of each.
//!
//! ```
//! let rf = ww_tape_example::rf::Rf::scaled(1000).expect("1000 divides the rate");
//! let piece = rf.record()?;
//! let read = ww_tape_example::rf::read(&piece.bytes)?;
//! assert_eq!(read.samples, rf.samples());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use core::f64::consts::PI;
use ww_tape::TrackNum;

use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::samples::{Samples, Scalar, encode, max_encoded_len};
use ww_tape::summary::{SeriesStats, SeriesSummarySpec};
use ww_tape::{
    BlockKind, ClockOrigin, ClockRef, ClockSpec, Codec, ColumnKind, ColumnSpec, Definitions,
    ElementDesc, EndMode, Items, Magnitude, MemSink, Ratio64, SampleShape, Segment, SeriesDef,
    SeriesGrid, SessionTag, SyncPolicy, TapeReader, TapeSink, TapeWriter, TrackKind, TrackRepeat,
    TrackSpec, clock_key, track_key,
};
use ww_tape_summary::{SeriesStart, SeriesSummaries};

use crate::signals::hash;

/// The root of the front end's API: its track and clock keys start from it.
const API_ROOT: u64 = 0x5d12_0001;
/// The shape of a sample of `iq`: one [IQ_ELEMENT].
pub const fn iq_shape() -> SampleShape {
    SampleShape {
        element: IQ_ELEMENT,
        channels: 1,
    }
}

// --8<-- [start:element]
/// The element of `iq`'s items: I and Q side by side, a 16-bit signed integer each.
pub const IQ_ELEMENT: ElementDesc = ElementDesc::of_scalar(Scalar::I16, 2);
// --8<-- [end:element]
/// The largest magnitude of a component: the converter's 12 bits, signed.
pub const FULL_SCALE: i16 = 2047;

/// Entries of the sine table the carriers read: a quarter of a turn is 1024 of them.
const TABLE_LEN: usize = 4096;
const _: () = assert!(TABLE_LEN == 4096, "the table's turn divides by 4096.0");
/// The carriers: each a frequency as a fraction of the rate (turns per sample, of 2^32) and an amplitude in steps.
const CARRIERS: [(i64, i64, i16); 3] = [(1, 8, 600), (-3, 20, 300), (7, 25, 150)];
/// The bursts: where each starts, ms into each minute of the capture.
pub const BURSTS_MS: [u32; 5] = [7_300, 19_040, 33_610, 41_220, 52_870];
/// How long a burst lasts, ms.
pub const BURST_MS: u32 = 2;
/// The burst's carrier, turns per sample, and its amplitude in steps.
const BURST: (i64, i64, i16) = (1, 16, 900);
/// The session of the capture's blocks.
const SESSION: u32 = 0x5d12;
/// The noise's seed.
const NOISE_SEED: u64 = 0x1f;

// --8<-- [start:rf_item]
/// An RF capture: its rate, its length and how it is stored.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rf {
    /// Complex samples per second; it divides 10^9, so a sample period is a whole number of ns.
    pub rate_hz: u32,
    /// Length of the capture, seconds.
    pub seconds: u32,
    /// Samples per series chunk.
    pub chunk_samples: u32,
    /// The codec of the samples.
    pub codec: Codec,
    /// `iq` carries summaries.
    pub summaries: bool,
}
// --8<-- [end:rf_item]

impl Rf {
    /// A minute at 20 MS/s, 80 MB/s of samples: chunks of 10 ms, the samples packed at 12 bits as the converter gives
    /// them ([Codec::PACKED12_LSB]), with summaries.
    pub const fn capture() -> Self {
        Self::at(20_000_000)
    }

    /// A minute at `rate_hz` samples a second, chunks of 10 ms.
    pub const fn at(rate_hz: u32) -> Self {
        Self {
            rate_hz,
            seconds: 60,
            chunk_samples: rate_hz / 100,
            codec: Codec::PACKED12_LSB,
            summaries: true,
        }
    }

    /// The capture at `1/k` of its rate: the same signal and chunks of the same time. None when `k` doesn't divide
    /// the rate into a whole number of samples per chunk.
    pub fn scaled(k: u32) -> Option<Self> {
        let full = Self::capture();
        if k == 0 || !full.rate_hz.is_multiple_of(k) || !full.chunk_samples.is_multiple_of(k) {
            return None;
        }
        Some(Self {
            rate_hz: full.rate_hz / k,
            chunk_samples: full.chunk_samples / k,
            ..full
        })
    }

    /// The same capture cut to `seconds`.
    pub const fn seconds(self, seconds: u32) -> Self {
        Self { seconds, ..self }
    }

    /// The same capture with its samples in `codec`.
    pub const fn codec(self, codec: Codec) -> Self {
        Self { codec, ..self }
    }

    /// Samples in the capture.
    pub fn count(&self) -> u64 {
        u64::from(self.rate_hz) * u64::from(self.seconds)
    }

    /// Ticks of the front end's sample clock (ns) from one sample to the next.
    pub fn period(&self) -> Option<Ratio64> {
        Ratio64::new(1_000_000_000, u64::from(self.rate_hz))
    }

    /// The track `iq`'s definition.
    pub fn def(&self) -> Option<SeriesDef<'static>> {
        // --8<-- [start:def]
        let def = SeriesDef {
            channels: UNib32(1), // one sample of IQ_ELEMENT at a time
            period: Ratio64::new(1_000_000_000, u64::from(self.rate_hz))?, // 50 ns at 20 MS/s
            scale: Magnitude::ONE, // one step of the converter
            summaries: self.summaries.then(summaries), // extremes and power per 1024 samples
            media: None,
        };
        // --8<-- [end:def]
        Some(def)
    }

    /// The front end's definitions: its sample clock in ns since the capture starts, and the track `iq` of
    /// `ComplexI16` samples on it; with the number `iq` was given.
    pub fn definitions<E>(&self) -> Result<(Definitions, TrackNum), RfError<E>> {
        let def = self.def().ok_or(RfError::Settings("the rate is 0"))?;
        let mut defs = Definitions::new();
        let clock = defs.define_clock(ClockSpec {
            key: clock_key(API_ROOT, &[Segment::Name("sample_clock")], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "front end ns",
        })?;
        let iq = defs.define_track(TrackSpec {
            key: iq_key(),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: Items::Numbers {
                element: IQ_ELEMENT,
                unit: None,
            },
            label: "iq",
            repeat: TrackRepeat::Stream,
            kind: TrackKind::Series(def),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        })?;
        Ok((defs, iq))
    }

    /// The signal of this capture.
    pub fn signal(&self) -> Signal {
        Signal::new(self.rate_hz)
    }

    /// Every sample of the capture, I and Q interleaved.
    pub fn samples(&self) -> Vec<i16> {
        let mut out = vec![0; usize::try_from(self.count() * 2).unwrap_or(0)];
        self.signal().fill(0, &mut out);
        out
    }

    /// Chunks in the capture.
    pub fn chunks(&self) -> u64 {
        self.count().div_ceil(u64::from(self.chunk_samples.max(1)))
    }

    /// The samples of chunk `k`: its first sample and its count.
    pub fn chunk_range(&self, k: u64) -> (u64, u32) {
        let first = k * u64::from(self.chunk_samples);
        let count = (self.count().saturating_sub(first)).min(u64::from(self.chunk_samples));
        (first, u32::try_from(count).unwrap_or(0))
    }

    /// Encode `values`, the samples from `first` on of track `iq`, into `bytes`: the chunk's head and its column,
    /// `bytes` cut to the column's length.
    pub fn encode_chunk<E>(
        &self,
        iq: TrackNum,
        first: u64,
        values: &[i16],
        bytes: &mut Vec<u8>,
    ) -> Result<(ChunkHead, ColumnSpec), RfError<E>> {
        let def = self.def().ok_or(RfError::Settings("the rate is 0"))?;
        let shape = iq_shape();
        let count = values.len() / 2;
        bytes.resize(max_encoded_len(self.codec, shape, count)?, 0);
        let len = encode(self.codec, shape, values, bytes)?;
        bytes.truncate(len);
        let count = u32::try_from(count).map_err(|_e| RfError::Settings("a chunk too long"))?;
        let grid: SeriesGrid = def.first_grid(0);
        let (first_tick, last_tick) = grid.chunk_ticks(def.period, first, count)?;
        let head = ChunkHead {
            kind: BlockKind::Series,
            track_id: iq_key(),
            track_number: iq,
            first_item: first,
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
            codec: self.codec,
            param: 0,
            len: bytes.len(),
        };
        Ok((head, spec))
    }

    /// The summaries of track `iq`, empty, when the capture has them.
    pub fn summary_builder<E>(
        &self,
        iq: TrackNum,
    ) -> Result<Option<SeriesSummaries<i16>>, RfError<E>> {
        let Some(def) = self.def().filter(|_| self.summaries) else {
            return Ok(None);
        };
        let start = SeriesStart {
            track_id: iq_key(),
            track_number: iq,
            shape: iq_shape(),
            first_item: 0,
            grid: def.first_grid(0),
            period: def.period,
        };
        Ok(Some(SeriesSummaries::new(start, summaries())?))
    }

    /// Record the capture as one piece into `sink`: what it wrote.
    pub fn record_on<S: TapeSink>(&self, sink: S) -> Result<Captured, RfError<S::Error>> {
        let (defs, iq) = self.definitions()?;
        let mut writer = TapeWriter::create_on(sink, SyncPolicy::None);
        // no clock and a fixed session: the same capture gives the same bytes
        writer.set_clock(ClockRef::NONE);
        writer.set_session(SessionTag::writable(SESSION))?;
        let mut summaries = self.summary_builder(iq)?;
        let signal = self.signal();
        let mut values = Vec::new();
        let mut bytes = Vec::new();
        let mut captured = Captured {
            rf: *self,
            len: 0,
            chunks: 0,
            sample_bytes: 0,
            summary_blocks: 0,
            summary_bytes: 0,
            bytes: Vec::new(),
        };
        for k in 0..self.chunks() {
            let (first, count) = self.chunk_range(k);
            values.resize(usize::try_from(count).unwrap_or(0) * 2, 0);
            signal.fill(first, &mut values);
            let (head, spec) = self.encode_chunk(iq, first, &values, &mut bytes)?;
            writer.write_chunk(&defs, &head, &[(spec, &bytes)])?;
            captured.chunks += 1;
            captured.sample_bytes += bytes.len() as u64;
            if let Some(s) = &mut summaries {
                s.push(&values, 0)?;
                for block in s.take_blocks() {
                    captured.summary_blocks += 1;
                    captured.summary_bytes += block.len() as u64;
                    writer.defer_block(&defs, block)?;
                }
            }
        }
        if let Some(s) = &mut summaries {
            s.finish()?;
            for block in s.take_blocks() {
                captured.summary_blocks += 1;
                captured.summary_bytes += block.len() as u64;
                writer.defer_block(&defs, block)?;
            }
        }
        captured.len = writer.close()?.len;
        Ok(captured)
    }

    /// Record the capture as one piece in memory: what it wrote, and the piece's bytes.
    pub fn record(&self) -> Result<Captured, RfError<ww_tape::MemError>> {
        let sink = MemSink::new();
        let tape = sink.tape();
        let mut captured = self.record_on(sink)?;
        captured.bytes = tape.to_vec().map_err(RfError::Mem)?;
        Ok(captured)
    }

    /// Record the capture as one piece into a file at `path`, created under its final name.
    pub fn write_file(
        &self,
        path: &std::path::Path,
    ) -> Result<Captured, RfError<ww_tape_fs::FsError>> {
        let sink = ww_tape_fs::FileSink::create(path).map_err(|e| {
            RfError::Write(ww_tape::WriteError::Sink {
                what: "creating the capture's file",
                error: e,
            })
        })?;
        self.record_on(sink)
    }
}

/// The key of the track `iq`.
pub fn iq_key() -> u64 {
    track_key(API_ROOT, &[Segment::Name("iq")])
}

/// The summaries of `iq`: per node of 1024 samples the smallest and largest I and Q, and the mean and largest power
/// `I² + Q²`.
pub fn summaries() -> SeriesSummarySpec {
    SeriesSummarySpec {
        base_shift: 10,
        stats: SeriesStats {
            min: true,
            max: true,
            sum: false,
            sum_sq: false,
            clipped: false,
            flags: false,
            power_mean: true,
            power_max: true,
        },
        clip: None,
    }
}

/// A capture written as one piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    /// What was captured.
    pub rf: Rf,
    /// Bytes of the piece, its close and tail included.
    pub len: u64,
    /// Series chunks in the piece.
    pub chunks: u64,
    /// Bytes of the chunks' sample columns.
    pub sample_bytes: u64,
    /// Summary blocks in the piece.
    pub summary_blocks: u64,
    /// Bytes of those summary blocks.
    pub summary_bytes: u64,
    /// The piece's bytes, when it was recorded in memory ([Rf::record]); else empty.
    pub bytes: Vec<u8>,
}

impl Captured {
    /// Bytes of the piece per sample, its chunks, summaries, definitions, index and close included.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a minute's bytes and samples are far below 2^52: exact in an f64"
    )]
    pub fn bytes_per_sample(&self) -> f64 {
        self.len as f64 / self.rf.count().max(1) as f64
    }
}

/// The IQ samples of a capture, read back from its piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadBack {
    /// Every sample, I and Q interleaved.
    pub samples: Vec<i16>,
    /// The chunks read.
    pub chunks: u64,
}

/// Read every sample of `iq` back from a piece: each chunk the index lists, its CRCs checked, its column decoded.
pub fn read(piece: &[u8]) -> Result<ReadBack, RfError<ww_tape::reader::OutOfRange>> {
    let reader = TapeReader::open(piece)?;
    let mut buf = Vec::new();
    let mut samples = Vec::new();
    let mut chunks = 0;
    for entry in reader.track_entries(0, iq_key())? {
        let block = reader.read_block(0, &entry, &mut buf)?;
        // the track's summary blocks are listed beside its chunks
        let Some(chunk) = block.chunk() else {
            continue;
        };
        let count = usize::try_from(chunk.head().count).unwrap_or(0);
        let column = chunk
            .find(ColumnKind::Samples, 0)
            .ok_or(RfError::Settings("a chunk of iq has samples"))?;
        let s = Samples::of(&column, iq_shape(), count)?;
        let at = samples.len();
        samples.resize(at + count * 2, 0);
        s.decode(0, samples.get_mut(at..).unwrap_or_default())?;
        chunks += 1;
    }
    Ok(ReadBack { samples, chunks })
}

/// The signal of a capture at a rate: the carriers' sine table and where the bursts fall.
#[derive(Clone, Debug)]
pub struct Signal {
    table: Vec<i32>,
    /// Samples in a minute.
    minute: u64,
    /// Each burst's samples in its minute, first to end.
    bursts: Vec<(u64, u64)>,
}

/// Turns per sample of `num/den` of the rate, of 2^32: a negative fraction turns the other way.
fn turns(num: i64, den: i64) -> u32 {
    let t = (num << 32) / den;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a turn modulo 2^32: the wrap is the phase"
    )]
    let v = t as u32;
    v
}

impl Signal {
    /// The signal at `rate_hz` samples a second.
    pub fn new(rate_hz: u32) -> Self {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a sine times 2^15 is within i32"
        )]
        let table = (0..TABLE_LEN)
            .map(|i| {
                let turn = f64::from(u16::try_from(i).unwrap_or(0)) / 4096.0;
                libm::round(libm::sin(2.0 * PI * turn) * 32_768.0) as i32
            })
            .collect();
        let at = |ms: u32| u64::from(ms) * u64::from(rate_hz) / 1_000;
        let bursts = BURSTS_MS
            .iter()
            .map(|&ms| (at(ms), at(ms + BURST_MS)))
            .collect();
        Self {
            table,
            minute: u64::from(rate_hz) * 60,
            bursts,
        }
    }

    /// `amplitude` times the sine and cosine of a carrier of `inc` turns per sample at sample `n`.
    #[inline(always)]
    fn carrier(&self, n: u64, inc: u32, amplitude: i16) -> (i32, i32) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the phase is the turn modulo 2^32"
        )]
        let phase = n.wrapping_mul(u64::from(inc)) as u32;
        let i = (phase >> 20) as usize;
        let sin = self.table.get(i).copied().unwrap_or(0);
        let cos = self
            .table
            .get((i + TABLE_LEN / 4) % TABLE_LEN)
            .copied()
            .unwrap_or(0);
        let a = i32::from(amplitude);
        ((a * cos) >> 15, (a * sin) >> 15)
    }

    /// Fill `out` with the samples from `first` on, I and Q interleaved: `out.len() / 2` samples.
    pub fn fill(&self, first: u64, out: &mut [i16]) {
        let carriers = CARRIERS.map(|(num, den, a)| (turns(num, den), a));
        for (n, iq) in (first..).zip(out.as_chunks_mut::<2>().0) {
            let (mut i, mut q) = (0, 0);
            for &(inc, a) in &carriers {
                let (ci, cq) = self.carrier(n, inc, a);
                i += ci;
                q += cq;
            }
            let noise = hash(NOISE_SEED, n);
            let gauss = |shift: u32| {
                // a sum of four bytes: near a normal of 148 steps, divided to about 6
                let b = |k: u32| {
                    i32::from(u8::try_from((noise >> (shift + 8 * k)) & 0xff).unwrap_or(0))
                };
                (b(0) + b(1) + b(2) + b(3) - 510) / 24
            };
            i += gauss(0);
            q += gauss(32);
            *iq = [component(i), component(q)];
        }
        let end = first + (out.len() / 2) as u64;
        let (num, den, a) = BURST;
        let inc = turns(num, den);
        let minutes = (first / self.minute.max(1))..end.div_ceil(self.minute.max(1));
        let bursts = minutes.flat_map(|m| {
            let at = m * self.minute;
            self.bursts.iter().map(move |&(b0, b1)| (at + b0, at + b1))
        });
        for (b0, b1) in bursts {
            for n in b0.max(first)..b1.min(end) {
                let (ci, cq) = self.carrier(n, inc, a);
                let k = usize::try_from(n - first).unwrap_or(0) * 2;
                if let Some([vi, vq]) = out.get_mut(k..k + 2) {
                    *vi = component(i32::from(*vi) + ci);
                    *vq = component(i32::from(*vq) + cq);
                }
            }
        }
    }
}

/// A component as the converter gives it: clipped to its 12 bits.
fn component(v: i32) -> i16 {
    let full = i32::from(FULL_SCALE);
    i16::try_from(v.clamp(-full, full)).unwrap_or(0)
}

/// Why a capture could not be recorded or read: `E` is the sink's or the source's error.
#[derive(Debug)]
#[non_exhaustive]
pub enum RfError<E = core::convert::Infallible> {
    /// A setting of [Rf] the capture can't be made with, or a piece that isn't one.
    Settings(&'static str),
    /// The front end's definitions.
    Describe(ww_tape::DescribeError),
    /// The samples of a chunk.
    Samples(ww_tape::SampleError),
    /// The grid of a chunk.
    Series(ww_tape::SeriesError),
    /// The summaries of `iq`.
    Summaries(ww_tape_summary::BuildError),
    /// The writer.
    Write(ww_tape::WriteError<E>),
    /// The piece in memory.
    Mem(ww_tape::MemError),
    /// Reading the piece back.
    Read(ww_tape::reader::ReadError<E>),
}

impl<E: core::fmt::Debug> core::fmt::Display for RfError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Settings(what) => write!(f, "the capture: {what}"),
            Self::Describe(e) => write!(f, "the front end's definitions: {e:?}"),
            Self::Samples(e) => write!(f, "the samples of iq: {e:?}"),
            Self::Series(e) => write!(f, "the grid of iq: {e:?}"),
            Self::Summaries(e) => write!(f, "the summaries of iq: {e:?}"),
            Self::Write(e) => write!(f, "writing the capture: {e:?}"),
            Self::Mem(e) => write!(f, "the piece in memory: {e:?}"),
            Self::Read(e) => write!(f, "reading the capture: {e:?}"),
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for RfError<E> {}

impl<E> From<ww_tape::DescribeError> for RfError<E> {
    fn from(e: ww_tape::DescribeError) -> Self {
        Self::Describe(e)
    }
}

impl<E> From<ww_tape::SampleError> for RfError<E> {
    fn from(e: ww_tape::SampleError) -> Self {
        Self::Samples(e)
    }
}

impl<E> From<ww_tape::SeriesError> for RfError<E> {
    fn from(e: ww_tape::SeriesError) -> Self {
        Self::Series(e)
    }
}

impl<E> From<ww_tape_summary::BuildError> for RfError<E> {
    fn from(e: ww_tape_summary::BuildError) -> Self {
        Self::Summaries(e)
    }
}

impl<E> From<ww_tape::WriteError<E>> for RfError<E> {
    fn from(e: ww_tape::WriteError<E>) -> Self {
        Self::Write(e)
    }
}

impl<E> From<ww_tape::reader::ReadError<E>> for RfError<E> {
    fn from(e: ww_tape::reader::ReadError<E>) -> Self {
        Self::Read(e)
    }
}

/// A sink that counts the bytes written and keeps none: a capture's size without its bytes in memory, and the
/// writer's own work without a medium's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Bytes written.
    pub bytes: u64,
}

impl TapeSink for Tally {
    type Error = std::io::Error;

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn size(&self) -> std::io::Result<u64> {
        Ok(self.bytes)
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> std::io::Result<()> {
        Err(unkept())
    }

    fn truncate(&mut self, _len: u64) -> std::io::Result<()> {
        Err(unkept())
    }

    fn rebase(&mut self, base: u64) -> std::io::Result<()> {
        if base == 0 { Ok(()) } else { Err(unkept()) }
    }
}

/// What a [Tally] answers a read, a cut or a rebase: it keeps no bytes.
fn unkept() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "a tally keeps no bytes")
}
