//! A wideband receiver at 1 GS/s, its channels spread over several muxes and chains in one process.
//!
//! Each channel is a series track with a producer thread of its own, and an overview reads the summaries only.
//!
//! The receiver digitizes a band at 1 billion complex samples a second, 4 GB/s as 16-bit I and Q. Its channelizer
//! splits the band into [Wideband::channels] channels of equal width, each at its share of the rate. Each channel is
//! the RF capture's signal ([crate::rf::Signal::channel]) at that rate, with noise of its own, as `ComplexI16` with
//! the summaries of [crate::rf::summaries].
//!
//! A track has one producer, so one thread encodes one channel's samples, hashes them and computes the leaves of
//! their summaries. Channel `k` goes to mux `k % muxes`, and each mux writes a chain of its own from its own thread,
//! into a directory on a disk of its own if the machine has several. [Wideband::record] runs a thread per channel.
//!
//! A reader that draws the whole capture reads summary nodes, not samples ([overview]): the nodes of one level of
//! each piece's tree, a few hundred bytes per node, against megabytes of samples per node.
//!
//! ```
//! use ww_tape_example::wideband::{Source, Wideband};
//! let wb = Wideband::scaled(100_000).expect("100 000 divides the rate").ms(2_000);
//! let tapes = ww_tape::MemTapes::new();
//! let mux = ww_tape_mux::TapeMux::create(
//!     tapes.clone(),
//!     ww_tape::OwnerId([0x5d; 32]),
//!     0,
//!     &[],
//!     ww_tape::ChainOptions::default(),
//!     ww_tape_mux::MuxOptions::default(),
//! )?;
//! let recorded = wb.record(&[&mux], &Source::Live)?;
//! let pieces = ww_tape_example::ethernet::pieces(&tapes, &mux.close()?)?;
//! let back = ww_tape_example::wideband::read_channel(&pieces, 3)?;
//! assert_eq!(back.len() as u64, 2 * recorded.samples[3]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use core::cell::Cell;

use ww_tape::reader::ByteSource;
use ww_tape::samples::Samples;
use ww_tape::{
    BlockKind, ChainOptions, Codec, ColumnKind, CutPolicy, ItemNum, Segment, SeriesDef, TapeReader,
    Tapes, TrackId, TrackKey, TrackNum, TrackSpec, track_key,
};
use ww_tape_mux::{MuxError, SeriesOptions, TapeMux};
use ww_tape_summary::Summary;

use crate::rf::{API_ROOT, Rf, RfError, Signal, clock, iq_shape};

/// The receiver's rate in all, complex samples a second.
pub const RATE_HZ: u32 = 1_000_000_000;
/// Channels of a receiver at most.
pub const CHANNELS_MAX: u32 = 64;

// --8<-- [start:wideband]
/// A capture of the wideband receiver: its rate, its channels, its length and how it is stored.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Wideband {
    /// Complex samples a second, of all channels together.
    pub rate_hz: u32,
    /// The channels the band is split into, each at `rate_hz / channels`: 1 to [CHANNELS_MAX].
    pub channels: u32,
    /// Length of the capture, ms.
    pub ms: u64,
    /// The codec of the samples.
    pub codec: Codec,
    /// Bytes of a piece of a chain at most: 0 for no cut by size.
    pub piece_bytes: u64,
}
// --8<-- [end:wideband]

/// Where a channel's thread takes its samples from.
#[derive(Clone, Debug)]
pub enum Source {
    /// The signal itself, made chunk by chunk as the thread goes.
    Live,
    /// Each channel's first chunks, made before and pushed again and again: what a bench takes, so that it times
    /// the recorder and not the signal's maths. The samples past the first chunks are not the signal's.
    Replay(Vec<Vec<Vec<i16>>>),
}

impl Wideband {
    /// A minute at 1 GS/s in 8 channels of 125 MS/s, `PACKED12_LSB`, pieces of 1 GiB.
    pub const fn full() -> Self {
        Self {
            rate_hz: RATE_HZ,
            channels: 8,
            ms: 60_000,
            codec: Codec::PACKED12_LSB,
            piece_bytes: 1 << 30,
        }
    }

    /// The capture at `1/k` of its rate, the same signal and pieces of `1/k` the size; None when `k` doesn't divide
    /// the rate of a channel.
    pub fn scaled(k: u32) -> Option<Self> {
        let full = Self::full();
        let channel = full.rate_hz.checked_div(full.channels)?;
        (k > 0 && channel.is_multiple_of(k)).then_some(Self {
            rate_hz: full.rate_hz / k,
            piece_bytes: full.piece_bytes / u64::from(k),
            ..full
        })
    }

    /// The same capture cut to `ms`.
    pub const fn ms(self, ms: u64) -> Self {
        Self { ms, ..self }
    }

    /// The same capture in `channels` channels.
    pub const fn channels(self, channels: u32) -> Self {
        Self { channels, ..self }
    }

    /// The same capture with its samples in `codec`.
    pub const fn codec(self, codec: Codec) -> Self {
        Self { codec, ..self }
    }

    /// Complex samples a second of one channel.
    pub fn channel_rate(&self) -> u32 {
        self.rate_hz / self.channels.max(1)
    }

    /// One channel as an RF capture: its rate, chunks of 10 ms, the codec and summaries.
    pub fn channel(&self) -> Rf {
        Rf {
            seconds: u32::try_from(self.ms.div_ceil(1_000)).unwrap_or(u32::MAX),
            codec: self.codec,
            ..Rf::at(self.channel_rate())
        }
    }

    /// Samples of one channel in the capture.
    pub fn channel_samples(&self) -> u64 {
        u64::from(self.channel_rate()) * self.ms / 1_000
    }

    /// The chain options of a mux: cut at [Self::piece_bytes], no sync, no checkpoint by time and no wall clock.
    pub fn chain_options(&self) -> ChainOptions {
        ChainOptions {
            cut: CutPolicy {
                bytes: exact_units::Bytes::new(self.piece_bytes),
                ..CutPolicy::default()
            },
            ..crate::ethernet::chain_options()
        }
    }

    /// Each channel's first `chunks` chunks, for [Source::Replay].
    pub fn replay(&self, chunks: u64) -> Source {
        let rf = self.channel();
        let rings = (0..self.channels)
            .map(|k| {
                let signal = Signal::channel(rf.rate_hz, k);
                (0..chunks.min(rf.chunks()))
                    .map(|c| {
                        let (first, count) = rf.chunk_range(c);
                        let mut values = vec![0; usize::try_from(count).unwrap_or(0) * 2];
                        signal.fill(first, &mut values);
                        values
                    })
                    .collect()
            })
            .collect();
        Source::Replay(rings)
    }

    /// Record the capture, channel `k` into the chain of `muxes[k % muxes.len()]`, each channel from a thread of its
    /// own taking its samples from `source`: the samples each channel pushed.
    ///
    /// # Errors
    ///
    /// A setting the capture can't be made with, or a mux or handle call that failed.
    pub fn record<T>(
        &self,
        muxes: &[&TapeMux<T>],
        source: &Source,
    ) -> Result<Recorded, WidebandError<T::Error>>
    where
        T: Tapes + Send + 'static,
        T::Sink: Send,
        T::Key: Send,
        T::Error: Send + Sync,
    {
        if !(1..=CHANNELS_MAX).contains(&self.channels)
            || !self.rate_hz.is_multiple_of(self.channels)
        {
            return Err(WidebandError::Settings(
                "1 to CHANNELS_MAX channels that divide the rate",
            ));
        }
        let tracks = define(self, muxes)?;
        let samples = std::thread::scope(|s| -> Result<_, WidebandError<T::Error>> {
            let threads: Vec<_> = tracks
                .iter()
                .map(|&(k, mux, track)| s.spawn(move || self.push_channel(mux, k, track, source)))
                .collect();
            threads
                .into_iter()
                .map(|t| {
                    t.join()
                        .map_err(|_e| WidebandError::Thread("a channel's thread"))?
                })
                .collect::<Result<Vec<u64>, _>>()
        })?;
        Ok(Recorded { samples })
    }

    /// The thread of channel `k`: push its samples into its track `track` on `mux`, chunk by chunk.
    fn push_channel<T>(
        &self,
        mux: &TapeMux<T>,
        k: u32,
        track: TrackNum,
        source: &Source,
    ) -> Result<u64, WidebandError<T::Error>>
    where
        T: Tapes + Send + 'static,
        T::Sink: Send,
        T::Key: Send,
        T::Error: Send + Sync,
    {
        let rf = self.channel();
        let chunks = self.channel_samples().div_ceil(u64::from(rf.chunk_samples));
        // --8<-- [start:channel]
        let mut iq = mux.series::<i16>(
            track,
            SeriesOptions {
                chunk_samples: rf.chunk_samples,
                latency: None, // the receiver fills every chunk
                codec: self.codec,
                ..SeriesOptions::default() // chunks of 1 MiB of samples at most, summaries on
            },
        )?;
        // --8<-- [end:channel]
        let signal = Signal::channel(rf.rate_hz, k);
        let ring = match source {
            Source::Live => None,
            Source::Replay(rings) => Some(
                rings
                    .get(usize::try_from(k).unwrap_or(usize::MAX))
                    .filter(|r| !r.is_empty())
                    .ok_or(WidebandError::Settings("a replay ring per channel"))?,
            ),
        };
        let mut values = Vec::new();
        let mut pushed = 0;
        for c in 0..chunks {
            let (first, count) = rf.chunk_range(c);
            let count = u64::from(count).min(self.channel_samples().saturating_sub(first));
            let n = usize::try_from(count * 2).unwrap_or(0);
            let chunk = match ring {
                Some(ring) => {
                    let at = usize::try_from(c % ring.len() as u64).unwrap_or(0);
                    ring.get(at)
                        .and_then(|r| r.get(..n))
                        .ok_or(WidebandError::Settings("a replayed chunk"))?
                }
                None => {
                    values.resize(n, 0);
                    signal.fill(first, &mut values);
                    &values
                }
            };
            iq.push(chunk)?;
            pushed += count;
        }
        iq.finish()?;
        Ok(pushed)
    }
}

/// The key of track `iq` of channel `k`.
pub fn channel_key(k: u32) -> TrackKey {
    track_key(
        API_ROOT,
        &[
            Segment::Name("channel"),
            Segment::Index(k),
            Segment::Name("iq"),
        ],
    )
}

/// The id of track `iq` of channel `k`.
pub fn channel_id(k: u32) -> TrackId {
    ww_tape::track_id(channel_key(k), [])
}

/// Track `iq` of channel `k` of `wb` on clock `clock`.
fn channel_spec(
    wb: &Wideband,
    k: u32,
    clock: ww_tape::ClockNum,
) -> Result<TrackSpec<'static>, RfError> {
    Ok(TrackSpec {
        key: channel_key(k),
        ..wb.channel().iq_track(clock)?
    })
}

/// A channel, the mux that writes it, and its track's number there.
type ChannelTrack<'m, T> = (u32, &'m TapeMux<T>, TrackNum);

/// Define the receiver's clock on each mux and each channel's track on its mux.
fn define<'m, T>(
    wb: &Wideband,
    muxes: &[&'m TapeMux<T>],
) -> Result<Vec<ChannelTrack<'m, T>>, WidebandError<T::Error>>
where
    T: Tapes + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    let mut clocks = Vec::new();
    for mux in muxes {
        clocks.push(mux.define_clock(clock())?);
    }
    let mut out = Vec::new();
    for k in 0..wb.channels {
        let at = usize::try_from(k).unwrap_or(usize::MAX) % muxes.len().max(1);
        let (mux, clock) = muxes
            .get(at)
            .zip(clocks.get(at))
            .ok_or(WidebandError::Settings("a mux"))?;
        let spec = channel_spec(wb, k, *clock).map_err(|e| WidebandError::Rf(e.to_string()))?;
        out.push((k, *mux, mux.define_track(spec)?));
    }
    Ok(out)
}

/// What [Wideband::record] pushed: the samples of each channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub samples: Vec<u64>,
}

/// Every sample of channel `k` in `pieces`, I and Q interleaved: each chunk its index lists, checked and decoded.
///
/// # Errors
///
/// A piece that doesn't read, or a chunk that doesn't decode.
pub fn read_channel(
    pieces: &[Vec<u8>],
    k: u32,
) -> Result<Vec<i16>, WidebandError<ww_tape::reader::OutOfRange>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for piece in pieces {
        let reader = TapeReader::open(piece.as_slice())?;
        for entry in reader.track_entries(0, channel_id(k))? {
            if entry.kind != BlockKind::Series {
                continue;
            }
            let block = reader.read_block(0, &entry, &mut buf)?;
            let chunk = block.chunk().ok_or(WidebandError::Read("a chunk"))?;
            let count = usize::try_from(chunk.head().count).unwrap_or(0);
            let column = chunk
                .find(ColumnKind::Samples, 0)
                .ok_or(WidebandError::Read("a chunk's samples"))?;
            let s = Samples::of(&column, iq_shape(), count)?;
            let at = out.len();
            out.resize(at + count * 2, 0);
            s.decode(0, out.get_mut(at..).unwrap_or_default())?;
        }
    }
    Ok(out)
}

/// A node of a channel's overview: the tick it starts at, the tick after its last sample, its samples, and their
/// mean and largest power `I² + Q²`.
///
/// Each piece has a tree of its own, so a piece's last node may hold fewer samples than a full node.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct OverviewNode {
    pub first_tick: i64,
    pub end_tick: i64,
    pub samples: u64,
    pub power_mean: f32,
    pub power_max: f32,
}

/// What [overview] read: the nodes in time order, and the bytes and reads it asked of the pieces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overview {
    pub nodes: Vec<OverviewNode>,
    pub bytes_read: u64,
    pub reads: u64,
}

/// A source that counts the bytes and the reads asked of it.
pub struct Counted<'c, S> {
    pub source: S,
    pub bytes: &'c Cell<u64>,
    pub reads: &'c Cell<u64>,
}

impl<S: ByteSource> ByteSource for Counted<'_, S> {
    type Error = S::Error;

    fn size(&self) -> Result<u64, S::Error> {
        self.source.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), S::Error> {
        self.bytes.set(self.bytes.get() + buf.len() as u64);
        self.reads.set(self.reads.get() + 1);
        self.source.read_at(offset, buf)
    }
}

// --8<-- [start:overview]
/// The overview of channel `k` of capture `wb` at summary level `level` from `pieces`: each piece opened, and only
/// the summary blocks of that level read.
///
/// # Errors
///
/// A piece that doesn't open, or a summary block that doesn't read.
pub fn overview<S: ByteSource>(
    wb: &Wideband,
    pieces: impl IntoIterator<Item = S>,
    k: u32,
    level: u8,
) -> Result<Overview, WidebandError<S::Error>> {
    let (bytes, reads) = (Cell::new(0), Cell::new(0));
    let def = wb
        .channel()
        .def()
        .ok_or(WidebandError::Settings("a rate"))?;
    let mut out = Overview::default();
    let mut buf = Vec::new();
    for source in pieces {
        let reader = TapeReader::open(Counted {
            source,
            bytes: &bytes,
            reads: &reads,
        })?;
        out.nodes.extend(summary_nodes(
            &reader,
            &def,
            channel_id(k),
            level,
            &mut buf,
        )?);
    }
    // --8<-- [end:overview]
    out.bytes_read = bytes.get();
    out.reads = reads.get();
    Ok(out)
}

// --8<-- [start:summary_nodes]
/// The summary nodes of level `level` of track `id`, a channel's `iq` of definition `def`, in the first piece of
/// `reader`: only the summary blocks of that level read, in order.
///
/// # Errors
///
/// A summary block that doesn't read.
pub fn summary_nodes<S: ByteSource>(
    reader: &TapeReader<S>,
    def: &SeriesDef<'_>,
    id: TrackId,
    level: u8,
    buf: &mut Vec<u8>,
) -> Result<Vec<OverviewNode>, WidebandError<S::Error>> {
    let grid = def.first_grid(0);
    let tick = |n: u64| {
        grid.tick(def.period, ItemNum::new(n))
            .ok_or(WidebandError::Read("a node's tick"))
    };
    let mut out = Vec::new();
    for entry in reader.track_entries(0, id)? {
        if entry.kind != BlockKind::Summary || entry.level != level {
            continue;
        }
        let block = reader.read_block(0, &entry, buf)?;
        let summary = Summary::of_block(&block).ok_or(WidebandError::Read("a summary"))?;
        let head = summary.head();
        for i in 0..summary.node_count() {
            let stats = summary
                .channel_stats(0, i)
                .ok_or(WidebandError::Read("a node"))?;
            // nodes lie on multiples of their size in item numbers: the first and last may be cut
            let (from, to) = (head.first_item.get(), head.first_item.get() + head.items);
            let start =
                (from >> head.node_shift << head.node_shift) + ((i as u64) << head.node_shift);
            let (first, end) = (start.max(from), (start + (1 << head.node_shift)).min(to));
            out.push(OverviewNode {
                first_tick: tick(first)?,
                end_tick: tick(end)?,
                samples: end.saturating_sub(first),
                power_mean: stats.power_mean,
                power_max: stats.power_max,
            });
        }
    }
    Ok(out)
}
// --8<-- [end:summary_nodes]

/// Why a capture could not be recorded or read: `E` is the store's or the source's error.
#[derive(Debug)]
#[non_exhaustive]
pub enum WidebandError<E = core::convert::Infallible> {
    /// A setting of [Wideband] the capture can't be made with.
    Settings(&'static str),
    /// A channel's track, as text.
    Rf(String),
    /// The mux or a handle.
    Mux(MuxError<E>),
    /// A channel's thread panicked.
    Thread(&'static str),
    /// Reading a piece.
    Open(ww_tape::reader::ReadError<E>),
    /// The samples of a chunk.
    Samples(ww_tape::SampleError),
    /// What a piece holds isn't what the capture writes.
    Read(&'static str),
}

impl<E: core::fmt::Debug> core::fmt::Display for WidebandError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Settings(what) => write!(f, "the capture: {what}"),
            Self::Rf(e) => write!(f, "{e}"),
            Self::Mux(e) => write!(f, "the mux: {e:?}"),
            Self::Thread(what) => write!(f, "{what} stopped"),
            Self::Open(e) => write!(f, "reading a piece: {e:?}"),
            Self::Samples(e) => write!(f, "the samples of a chunk: {e:?}"),
            Self::Read(what) => write!(f, "reading a piece: {what}"),
        }
    }
}

impl<E: core::fmt::Debug> core::error::Error for WidebandError<E> {}

impl<E> From<MuxError<E>> for WidebandError<E> {
    fn from(e: MuxError<E>) -> Self {
        Self::Mux(e)
    }
}

impl<E> From<ww_tape::reader::ReadError<E>> for WidebandError<E> {
    fn from(e: ww_tape::reader::ReadError<E>) -> Self {
        Self::Open(e)
    }
}

impl<E> From<ww_tape::SampleError> for WidebandError<E> {
    fn from(e: ww_tape::SampleError) -> Self {
        Self::Samples(e)
    }
}
