//! The RF capture in two chains: the samples in one, what a decoder finds in them in another, each written through a
//! mux of its own in one process, and a reader that follows both and merges them by time.
//!
//! A radio thread pushes each chunk's samples into a series handle of the chain `iq` and hands them to a decoder
//! thread. The decoder looks for bursts and measures the band ([Detector]), and pushes what it finds into two events
//! handles of the chain `decoded`: track `bursts`, a span per burst, and track `power`, the band's mean power each
//! second. The chains live in two directories of piece files ([Chains::write]).
//!
//! A reader in another thread follows both chains while they are written ([follow]). The samples' chunks and the
//! decoded items come out of a [TimeMerge] in time order: each burst right after the chunk its first sample is in.
//!
//! ```
//! let rf = example_capture::rf::Rf::scaled(1000).expect("1000 divides the rate").seconds(10);
//! let dir = std::env::temp_dir().join(format!("ww_tape_rf_chains_doc_{}", std::process::id()));
//! let written = example_rf::chains::Chains::new(rf).write_followed(&dir)?;
//! assert_eq!(written.followed.bursts.len(), 1, "a burst at 7.3 s");
//! std::fs::remove_dir_all(&dir)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use exact_units::Count;
use shrink_wrap::prelude::*;
use ww_tape::index::IndexPolicy;
use ww_tape::{
    ChainOptions, ClockNum, ClockRef, CutPolicy, Ends, EventsDef, ItemNum, Items, OwnerId,
    PieceTapes, Segment, SyncPolicy, TrackKey, TrackKind, TrackNum, TrackRepeat, TrackSpec,
    WriterOptions, track_key,
};
use ww_tape_fs::follow::{
    ChainFollower, FollowError, FollowOptions, TimeMerge, TrackItem, TrackItems,
};
use ww_tape_fs::{Files, FsError};
use ww_tape_mux::{EventsHandle, EventsOptions, MuxError, MuxOptions, SeriesOptions, TapeMux};

use example_capture::rf::{API_ROOT, Rf, RfError, clock, iq_key};

/// The owner of both chains: one process writes them.
const OWNER: OwnerId = OwnerId([0x5d; 32]);
/// The number of the chain of the samples, and of the chain of what the decoder finds.
pub const IQ_CHAIN: u64 = 0;
pub const DECODED_CHAIN: u64 = 1;
/// Chunks of the samples the radio thread hands the decoder and the decoder hasn't taken, at most.
pub const DECODER_QUEUE_MAX: usize = 4;
/// Samples the decoder averages the power over: a burst's carrier turns a whole number of times in it.
pub const WINDOW_SAMPLES: u64 = 16;
/// The mean power `I² + Q²` of a window at or above which it is part of a burst: the carriers alone stay near
/// 472 500, with a burst near 1 280 000.
pub const BURST_POWER_MIN: u64 = 840_000;
/// How long the reader waits on one chain before it looks at the other again.
const FOLLOW_WAIT: Duration = Duration::from_millis(50);

/// The key of the track `bursts`.
pub fn bursts_key() -> TrackKey {
    track_key(API_ROOT, &[Segment::Name("bursts")])
}

/// The key of the track `power`.
pub fn power_key() -> TrackKey {
    track_key(API_ROOT, &[Segment::Name("power")])
}

/// An events track of the decoder on clock `clock`: `bursts` spans, `power` instants; the payload a little-endian
/// `u32` power either way.
fn decoded_track(
    key: TrackKey,
    label: &'static str,
    ends: Ends,
    clock: ClockNum,
) -> TrackSpec<'static> {
    TrackSpec {
        key,
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label,
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef { ends, flags: false }),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// What the decoder finds, in time order.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A burst: the ticks (ns) of its first window and past its last, and the largest mean power of a window.
    Burst { start: i64, end: i64, power: u32 },
    /// The band's mean power over the second that ends at tick `at`.
    Power { at: i64, power: u32 },
}

impl Decoded {
    /// The tick it starts at.
    pub fn start(&self) -> i64 {
        match self {
            Self::Burst { start, .. } => *start,
            Self::Power { at, .. } => *at,
        }
    }
}

/// Finds bursts in a capture's samples and measures the band's power each second, sample by sample.
///
/// It gives its finds in the order they start: the power of a second that ends during a burst waits for the burst.
#[derive(Clone, Debug)]
pub struct Detector {
    /// Ticks from one sample to the next.
    period_ns: i64,
    /// Samples in a second.
    rate: u64,
    /// The next sample's number.
    next: u64,
    /// The sum of the power of the open window's samples, and of the open second's.
    window: u64,
    second: u64,
    /// The burst going on: its first tick and its largest window power.
    open: Option<(i64, u64)>,
    /// Seconds measured while a burst goes on: their end tick and mean power.
    held: Vec<(i64, u32)>,
}

impl Detector {
    /// A detector for capture `rf`, from its first sample; None when its period isn't a whole number of ns.
    pub fn new(rf: &Rf) -> Option<Self> {
        let rate = u64::from(rf.rate_hz);
        let period_ns = i64::try_from(1_000_000_000_u64.checked_div(rate)?).ok()?;
        (rate > 0 && 1_000_000_000 % rate == 0).then_some(Self {
            period_ns,
            rate,
            next: 0,
            window: 0,
            second: 0,
            open: None,
            held: Vec::new(),
        })
    }

    /// The tick of sample `n`.
    fn tick(&self, n: u64) -> i64 {
        i64::try_from(n)
            .unwrap_or(i64::MAX)
            .saturating_mul(self.period_ns)
    }

    /// Take the next samples, I and Q interleaved, and add what they end to `out`.
    pub fn push(&mut self, values: &[i16], out: &mut Vec<Decoded>) {
        for [i, q] in values.as_chunks::<2>().0 {
            let power = (i32::from(*i).pow(2) + i32::from(*q).pow(2)).unsigned_abs();
            self.window += u64::from(power);
            self.second += u64::from(power);
            self.next += 1;
            if self.next.is_multiple_of(WINDOW_SAMPLES) {
                let mean = self.window / WINDOW_SAMPLES;
                self.window = 0;
                let start = self.tick(self.next - WINDOW_SAMPLES);
                self.open = match (self.open, mean >= BURST_POWER_MIN) {
                    (Some((first, peak)), true) => Some((first, peak.max(mean))),
                    (None, true) => Some((start, mean)),
                    (Some((first, peak)), false) => {
                        out.push(Decoded::Burst {
                            start: first,
                            end: start,
                            power: u32::try_from(peak).unwrap_or(u32::MAX),
                        });
                        out.extend(
                            self.held
                                .drain(..)
                                .map(|(at, power)| Decoded::Power { at, power }),
                        );
                        None
                    }
                    (None, false) => None,
                };
            }
            if self.next.is_multiple_of(self.rate) {
                let mean = u32::try_from(self.second / self.rate).unwrap_or(u32::MAX);
                self.second = 0;
                let at = self.tick(self.next);
                if self.open.is_some() {
                    self.held.push((at, mean));
                } else {
                    out.push(Decoded::Power { at, power: mean });
                }
            }
        }
    }
}

/// The two chains of a capture: where they are and how they are cut.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Chains {
    pub rf: Rf,
    /// Chunks of the samples a piece of `iq` holds: a second's by default.
    pub iq_piece_chunks: u64,
}

/// How a chain of the capture is written: cut every `chunks` chunk blocks, no sync, no checkpoint by time and no
/// clock, so the same capture gives the same bytes.
fn chain_options(chunks: u64) -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            chunks: Count::new(chunks),
            ..CutPolicy::default()
        },
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                interval: None,
                ..IndexPolicy::default()
            },
            clock: ClockRef::NONE,
            ..WriterOptions::default()
        },
        tapes: PieceTapes::Each,
    }
}

/// The chains of a capture, started ([Chains::open]): a mux each, and the numbers of their tracks.
pub struct Open {
    pub iq_mux: TapeMux<Files>,
    pub decoded_mux: TapeMux<Files>,
    pub iq: TrackNum,
    pub bursts: TrackNum,
    pub power: TrackNum,
}

/// What [Chains::write] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// The pieces of `iq` and their bytes.
    pub iq_pieces: u64,
    pub iq_bytes: u64,
    /// The pieces of `decoded` and their bytes.
    pub decoded_pieces: u64,
    pub decoded_bytes: u64,
    /// What the decoder found.
    pub decoded: Vec<Decoded>,
}

/// What [Chains::write_followed] wrote, and what the reader read while it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenFollowed {
    pub written: Written,
    pub followed: Followed,
}

impl Chains {
    /// The capture `rf` in two chains, a piece of `iq` per second.
    pub const fn new(rf: Rf) -> Self {
        Self {
            rf,
            iq_piece_chunks: 100,
        }
    }

    /// The directory of chain `iq` under `dir`.
    pub fn iq_dir(dir: &Path) -> PathBuf {
        dir.join("iq")
    }

    /// The directory of chain `decoded` under `dir`.
    pub fn decoded_dir(dir: &Path) -> PathBuf {
        dir.join("decoded")
    }

    /// Write the capture into the chains `iq` and `decoded` under `dir`, as the [module docs](self) say.
    ///
    /// # Errors
    ///
    /// A chain that didn't start, or the radio's, the decoder's or a mux's error.
    pub fn write(&self, dir: &Path) -> Result<Written, ChainsError> {
        self.record(self.open(dir)?)
    }

    /// Write the capture as [Self::write] does while a reader in another thread follows both chains ([follow]).
    pub fn write_followed(&self, dir: &Path) -> Result<WrittenFollowed, ChainsError> {
        let (rf, iq, decoded) = (self.rf, Self::iq_dir(dir), Self::decoded_dir(dir));
        let open = self.open(dir)?;
        std::thread::scope(|s| {
            let reader = s.spawn(|| follow(&rf, &iq, &decoded));
            let written = self.record(open)?;
            let followed = reader
                .join()
                .map_err(|_e| ChainsError::Thread("the reader"))??;
            Ok(WrittenFollowed { written, followed })
        })
    }

    /// Start the chains `iq` and `decoded` under `dir`, a mux each, their clock and tracks defined: what a reader
    /// follows from now on.
    ///
    /// # Errors
    ///
    /// A directory or a mux that didn't start, or a definition refused.
    pub fn open(&self, dir: &Path) -> Result<Open, ChainsError> {
        let rf = self.rf;
        for d in [Self::iq_dir(dir), Self::decoded_dir(dir)] {
            std::fs::create_dir_all(d).map_err(ChainsError::Io)?;
        }
        // --8<-- [start:two_muxes]
        let iq_mux = ww_tape_fs::mux::create(
            &Self::iq_dir(dir),
            OWNER,
            IQ_CHAIN,
            &[],
            chain_options(self.iq_piece_chunks),
            MuxOptions::default(),
        )?;
        let decoded_mux = ww_tape_fs::mux::create(
            &Self::decoded_dir(dir),
            OWNER,
            DECODED_CHAIN,
            &[],
            chain_options(0), // a minute's finds fit one piece
            MuxOptions::default(),
        )?;
        let iq = iq_mux.define_track(rf.iq_track::<FsError>(iq_mux.define_clock(clock())?)?)?;
        let c = decoded_mux.define_clock(clock())?;
        let bursts =
            decoded_mux.define_track(decoded_track(bursts_key(), "bursts", Ends::Spans, c))?;
        let power =
            decoded_mux.define_track(decoded_track(power_key(), "power", Ends::Instants, c))?;
        // --8<-- [end:two_muxes]
        Ok(Open {
            iq_mux,
            decoded_mux,
            iq,
            bursts,
            power,
        })
    }

    /// Record the capture into the chains [Self::open] started: a radio thread pushes the samples into `iq` and
    /// hands them to a decoder thread, which pushes what it finds into `decoded`. Both chains close at the end.
    ///
    /// # Errors
    ///
    /// The radio's, the decoder's or a mux's error.
    pub fn record(&self, open: Open) -> Result<Written, ChainsError> {
        let rf = self.rf;
        let Open {
            iq_mux,
            decoded_mux,
            iq,
            bursts,
            power,
        } = open;
        // --8<-- [start:threads]
        let mut samples = iq_mux.series::<i16>(
            iq,
            SeriesOptions {
                chunk_samples: rf.chunk_samples,
                latency: None, // the radio fills every chunk
                codec: rf.codec,
                ..SeriesOptions::default()
            },
        )?;
        let finds = |track| {
            decoded_mux.events(
                track,
                EventsOptions {
                    latency: None, // the decoder completes each second's finds
                    ..EventsOptions::default()
                },
            )
        };
        let (mut bursts, mut power) = (finds(bursts)?, finds(power)?);
        let mut detector = Detector::new(&rf).ok_or(ChainsError::Rf(RfError::Settings(
            "the period is a whole number of ns",
        )))?;
        let (to_decoder, from_radio) = mpsc::sync_channel::<Vec<i16>>(DECODER_QUEUE_MAX);
        let decoded = std::thread::scope(|s| {
            let radio = s.spawn(move || -> Result<(), ChainsError> {
                let signal = rf.signal();
                for k in 0..rf.chunks() {
                    let (first, count) = rf.chunk_range(k);
                    let mut values = vec![0; usize::try_from(count).unwrap_or(0) * 2];
                    signal.fill(first, &mut values);
                    samples.push(&values)?;
                    to_decoder
                        .send(values)
                        .map_err(|_e| ChainsError::Thread("the decoder"))?;
                }
                Ok(samples.finish()?)
            });
            let decoder = s.spawn(move || -> Result<Vec<Decoded>, ChainsError> {
                let (mut all, mut found) = (Vec::new(), Vec::new());
                for values in from_radio {
                    detector.push(&values, &mut found);
                    for d in found.drain(..) {
                        put(&mut bursts, &mut power, d)?;
                        all.push(d);
                    }
                }
                bursts.finish()?;
                power.finish()?;
                Ok(all)
            });
            let radio = radio.join().map_err(|_e| ChainsError::Thread("the radio"));
            let decoded = decoder
                .join()
                .map_err(|_e| ChainsError::Thread("the decoder"));
            radio??;
            decoded?
        })?;
        let (iq_pieces, decoded_pieces) = (iq_mux.close()?, decoded_mux.close()?);
        // --8<-- [end:threads]
        let bytes = |p: &[ww_tape::ChainPiece<u64>]| p.iter().map(|p| p.piece.len).sum();
        Ok(Written {
            iq_pieces: iq_pieces.len() as u64,
            iq_bytes: bytes(&iq_pieces),
            decoded_pieces: decoded_pieces.len() as u64,
            decoded_bytes: bytes(&decoded_pieces),
            decoded,
        })
    }
}

/// Push find `d` into its track; a power closes a second, so the second's finds are completed and sent: the reader
/// gets them in time order, a second at a time.
fn put(
    bursts: &mut EventsHandle<Files>,
    power: &mut EventsHandle<Files>,
    d: Decoded,
) -> Result<(), ChainsError> {
    match d {
        Decoded::Burst {
            start,
            end,
            power: p,
        } => {
            bursts.push(start, end, 0, &p.to_le_bytes())?;
        }
        Decoded::Power { at, power: p } => {
            bursts.complete()?;
            power.push(at, at, 0, &p.to_le_bytes())?;
            power.complete()?;
        }
    }
    Ok(())
}

/// What the reader read from both chains, merged by time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Followed {
    /// Chunks of `iq` read, and their samples.
    pub chunks: u64,
    pub samples: u64,
    /// The items of `decoded` in the order the merge gave them.
    pub decoded: Vec<Decoded>,
    /// Each burst read with the first sample of the chunk of `iq` the merge gave right before it.
    pub bursts: Vec<(Decoded, u64)>,
    /// Items the merge gave out of time order: none, since it waits for both chains.
    pub late: u64,
}

/// An item of either chain, as the merge takes it.
#[derive(Clone, Debug)]
enum Item {
    Chunk { first: ItemNum, count: u32 },
    Decoded(Decoded),
}

/// Follow the chains `iq` and `decoded` of capture `rf` in `iq_dir` and `decoded_dir` until both are closed, and
/// merge their items by time.
pub fn follow(rf: &Rf, iq_dir: &Path, decoded_dir: &Path) -> Result<Followed, ChainsError> {
    let period = rf.period().ok_or(RfError::Settings("the rate is 0"))?;
    // --8<-- [start:follow]
    let mut iq = ChainFollower::new(iq_dir, FollowOptions::default())?.track(iq_key().id());
    let mut decoded = ChainFollower::new(decoded_dir, FollowOptions::default())?;
    let (mut bursts, mut power) = (
        TrackItems::new(bursts_key().id()),
        TrackItems::new(power_key().id()),
    );
    let now = Instant::now();
    let mut merge = TimeMerge::new(None); // exact order: wait for both chains
    let (from_iq, from_decoded) = (merge.add_source(now), merge.add_source(now));
    let mut out = Followed::default();
    let mut last_chunk = None;
    // a block or item a wait brought, taken first in the next round
    let (mut held_iq, mut held_decoded) = (None, None);
    loop {
        let mut got = false;
        // the samples' chunks, each at the tick of its first sample
        while let Some(item) = held_iq
            .take()
            .map_or_else(|| iq.try_next(), |i| Ok(Some(i)))?
        {
            let TrackItem::Samples(s) = item else {
                return Err(ChainsError::Read("iq holds samples, none lost"));
            };
            let tick = s
                .grid
                .tick(period, s.first)
                .ok_or(ChainsError::Read("a chunk's tick"))?;
            let chunk = Item::Chunk {
                first: s.first,
                count: s.count,
            };
            merge.push(from_iq, i128::from(tick), chunk, now);
            got = true;
        }
        // the decoder's finds: two tracks sent a second at a time, in time order, so one source
        while let Some(f) = held_decoded
            .take()
            .map_or_else(|| decoded.try_next(), |f| Ok(Some(f)))?
        {
            let mut items = VecDeque::new();
            bursts.take(&f.block, decoded.definitions(), &mut items)?;
            power.take(&f.block, decoded.definitions(), &mut items)?;
            for d in items.into_iter().map(decoded_item) {
                let d = d?;
                merge.push(from_decoded, i128::from(d.start()), Item::Decoded(d), now);
            }
            got = true;
        }
        // a closed chain holds nothing more: past every time
        for (source, closed) in [
            (from_iq, iq.chain().is_closed()),
            (from_decoded, decoded.is_closed()),
        ] {
            if closed {
                merge.mark(source, i128::MAX, now);
            }
        }
        while let Some(m) = merge.pop(now) {
            out.late += u64::from(m.late);
            match m.item {
                Item::Chunk { first, count } => {
                    out.chunks += 1;
                    out.samples += u64::from(count);
                    last_chunk = Some(first.get());
                }
                Item::Decoded(d) => {
                    if let (Decoded::Burst { .. }, Some(first)) = (d, last_chunk) {
                        out.bursts.push((d, first));
                    }
                    out.decoded.push(d);
                }
            }
        }
        if iq.chain().is_closed() && decoded.is_closed() && merge.waiting() == 0 {
            break;
        }
        if !got {
            // nothing new in either chain: wait for the decoder's next second, which holds the merge back, or for
            // the samples once the decoder is done
            if decoded.is_closed() {
                held_iq = iq.wait_next_for(FOLLOW_WAIT)?;
            } else {
                held_decoded = decoded.wait_next_for(FOLLOW_WAIT)?;
            }
        }
    }
    // --8<-- [end:follow]
    Ok(out)
}

/// A find from an item of `bursts` or `power`.
fn decoded_item(item: TrackItem) -> Result<Decoded, ChainsError> {
    let TrackItem::Event {
        start,
        end,
        payload,
        ..
    } = item
    else {
        return Err(ChainsError::Read("decoded holds events, none lost"));
    };
    let p = u32::from_le_bytes(
        payload
            .try_into()
            .map_err(|_e| ChainsError::Read("a find's power is 4 bytes"))?,
    );
    Ok(if start == end {
        Decoded::Power {
            at: start,
            power: p,
        }
    } else {
        Decoded::Burst {
            start,
            end,
            power: p,
        }
    })
}

/// Why the chains could not be written or followed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ChainsError {
    /// A setting of the capture.
    Rf(RfError<FsError>),
    /// A mux.
    Mux(MuxError<FsError>),
    /// Following a chain.
    Follow(FollowError),
    /// A chunk of a followed track.
    Track(ww_tape_fs::follow::TrackError),
    /// What a chain holds isn't what the capture writes.
    Read(&'static str),
    /// A thread of the capture panicked or stopped early.
    Thread(&'static str),
    /// Making a chain's directory.
    Io(std::io::Error),
}

impl core::fmt::Display for ChainsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Rf(e) => write!(f, "{e}"),
            Self::Mux(e) => write!(f, "a mux: {e:?}"),
            Self::Follow(e) => write!(f, "{e}"),
            Self::Track(e) => write!(f, "{e}"),
            Self::Read(what) => write!(f, "reading the chains: {what}"),
            Self::Thread(what) => write!(f, "{what} stopped"),
            Self::Io(e) => write!(f, "making a chain's directory: {e}"),
        }
    }
}

impl core::error::Error for ChainsError {}

impl From<RfError<FsError>> for ChainsError {
    fn from(e: RfError<FsError>) -> Self {
        Self::Rf(e)
    }
}

impl From<MuxError<FsError>> for ChainsError {
    fn from(e: MuxError<FsError>) -> Self {
        Self::Mux(e)
    }
}

impl From<FollowError> for ChainsError {
    fn from(e: FollowError) -> Self {
        Self::Follow(e)
    }
}

impl From<ww_tape_fs::follow::TrackError> for ChainsError {
    fn from(e: ww_tape_fs::follow::TrackError) -> Self {
        Self::Track(e)
    }
}
