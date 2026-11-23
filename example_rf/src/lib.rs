//! The RF capture: complex IQ samples from a software-defined radio's front end, the book's case for high rates.
//!
//! The signal and the track `iq` are [example_capture::rf]'s: three steady carriers, noise and five bursts of 2 ms a
//! minute, a pure function of the sample's number, so every capture is the same. This crate records it.
//!
//! [record_on] writes the capture as one piece into any sink: one track `iq`, a series chunk per
//! [Rf::chunk_samples], and summary nodes of 1024 samples with the mean and the largest power of each. [chains]
//! writes it into two chains from two threads, the bursts a decoder finds in the second, and follows both.
//!
//! ```
//! let rf = example_capture::rf::Rf::scaled(1000).expect("1000 divides the rate");
//! let piece = example_rf::record(&rf)?;
//! let read = example_rf::read(&piece.bytes)?;
//! assert_eq!(read.samples, rf.samples());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    reason = "the case computes times, counts and offsets of a capture it makes itself from its settings, far from any \
              limit, and reads no bytes of others"
)]

pub mod chains;

use example_capture::rf::{Rf, RfError, iq_key, iq_shape};
use ww_tape::samples::Samples;
use ww_tape::{
    ClockRef, ColumnKind, MemSink, SessionTag, SyncPolicy, TapeReader, TapeSink, TapeWriter,
};

/// The session of the capture's blocks.
const SESSION: u32 = 0x5d12;

/// Record the capture as one piece into `sink`: what it wrote.
pub fn record_on<S: TapeSink>(rf: &Rf, sink: S) -> Result<Captured, RfError<S::Error>> {
    let (defs, iq) = rf.definitions()?;
    let mut writer = TapeWriter::create_on(sink, SyncPolicy::None);
    // no clock and a fixed session: the same capture gives the same bytes
    writer.set_clock(ClockRef::NONE);
    writer.set_session(SessionTag::writable(SESSION))?;
    let mut summaries = rf.summary_builder(iq)?;
    let signal = rf.signal();
    let mut values = Vec::new();
    let mut bytes = Vec::new();
    let mut captured = Captured {
        rf: *rf,
        len: 0,
        chunks: 0,
        sample_bytes: 0,
        summary_blocks: 0,
        summary_bytes: 0,
        bytes: Vec::new(),
    };
    for k in 0..rf.chunks() {
        let (first, count) = rf.chunk_range(k);
        values.resize(usize::try_from(count).unwrap_or(0) * 2, 0);
        signal.fill(first, &mut values);
        let (head, spec) = rf.encode_chunk(iq, first, &values, &mut bytes)?;
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
pub fn record(rf: &Rf) -> Result<Captured, RfError<ww_tape::MemError>> {
    let sink = MemSink::new();
    let tape = sink.tape();
    let mut captured = record_on(rf, sink)?;
    captured.bytes = tape.to_vec().map_err(RfError::Mem)?;
    Ok(captured)
}

/// Record the capture as one piece into a file at `path`, created under its final name.
pub fn write_file(
    rf: &Rf,
    path: &std::path::Path,
) -> Result<Captured, RfError<ww_tape_fs::FsError>> {
    let sink = ww_tape_fs::FileSink::create(path).map_err(|e| {
        RfError::Write(ww_tape::WriteError::Sink {
            what: "creating the capture's file",
            error: e,
        })
    })?;
    record_on(rf, sink)
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
    for entry in reader.track_entries(0, iq_key().id())? {
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
