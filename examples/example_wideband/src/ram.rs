//! A receiver at 250 MS/s captured into memory, only the stretches around its bursts kept.
//!
//! [RfCapture::run] records the receiver as [Wideband::record] does, into a chain of pieces in memory
//! ([example_capture::window]). Its analyser, [Bursts], detects energy: it reads each piece's level-0 summary nodes, never its
//! samples. A node whose largest power `I² + Q²` passes [RfCapture::power_min] holds a burst. A piece with such a
//! node is kept, with [Window::before] pieces before it and [Window::after] after it; the rest leave memory unwritten.
//!
//! The analyser's results go to a chain of their own, through `results`, a mux on any store:
//!
//! | Track | Item | Payload |
//! |---|---|---|
//! | `bursts` of each channel | a burst, from the first node that holds it to the end of the last one | its largest power, f32 LE |
//! | `kept` | a piece kept, from its first item's tick to its last one's ([KeptLog]) | the piece's sequence number, 8 bytes LE |
//!
//! A burst may span pieces: the analyser carries it from one piece to the next and pushes it once a quiet node ends
//! it.

use shrink_wrap::prelude::*;
use ww_tape::{
    ClockNum, Codec, Ends, EventsDef, GridDef, Items, MemError, MemPiece, PieceStore, Segment,
    TapeReader, TrackId, TrackKind, TrackRepeat, TrackSpec, track_key,
};
use ww_tape_mux::{Event, EventsHandle, EventsOptions, MuxError, TapeMux};

use crate::{Recorded, Source, Wideband, WidebandError, channel_id, summary_nodes};
use example_capture::rf::clock;
use example_capture::window::{
    self, Analyser, KeptLog, LookError, MemChain, RunError, Triaged, Window, kept_spec,
};

/// The root of the results' API: their track and clock keys start from it.
pub const RESULTS_ROOT: u64 = 0x5d_0002;
/// The receiver's rate in all, complex samples a second.
pub const RATE_HZ: u32 = 250_000_000;
/// The largest power of a node of a quiet band stays below this: the carriers' amplitudes add up to 1 050 steps,
/// and noise adds a few. A burst adds 900 steps.
pub const POWER_MIN: f32 = 1.5e6;
/// The capture's owner.
const OWNER: ww_tape::OwnerId = ww_tape::OwnerId([0x5d; 32]);

// --8<-- [start:rf_capture]
/// A capture of the receiver into a window in memory, the pieces around each burst kept.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RfCapture {
    /// The receiver: its rate, channels, length, codec and pieces.
    pub wb: Wideband,
    pub window: Window,
    /// The largest power of a summary node that holds a burst, at least.
    pub power_min: f32,
}
// --8<-- [end:rf_capture]

impl RfCapture {
    /// A minute at 250 MS/s in 2 channels of 125 MS/s, `PACKED12_LSB`, pieces of 64 MiB, a window of 8 GiB, the piece
    /// before and the piece after a burst kept with it.
    pub const fn full() -> Self {
        Self {
            wb: Wideband {
                rate_hz: RATE_HZ,
                channels: 2,
                ms: 60_000,
                codec: Codec::PACKED12_LSB,
                piece_bytes: 64 << 20,
            },
            window: Window {
                bytes_max: 8 << 30,
                before: 1,
                after: 1,
            },
            power_min: POWER_MIN,
        }
    }

    /// The capture at `1/k` of its rate, the same signal, pieces of `1/k` the size; None when `k` doesn't divide the
    /// rate of a channel.
    pub fn scaled(k: u32) -> Option<Self> {
        let full = Self::full();
        let channel = full.wb.channel_rate();
        (k > 0 && channel.is_multiple_of(k)).then_some(Self {
            wb: Wideband {
                rate_hz: full.wb.rate_hz / k,
                piece_bytes: full.wb.piece_bytes / u64::from(k),
                ..full.wb
            },
            ..full
        })
    }

    /// Start recording the receiver into memory from `source`, the pieces around each burst kept in `kept` and the
    /// analyser's results pushed through `results`, as the [module docs](self) say. The capture runs on threads of
    /// its own; the [RfRunning] it gives hands out the memory window while it runs.
    ///
    /// # Errors
    ///
    /// A channel's rate without a sample period, a results' definition or handle refused, or a mux or thread that
    /// didn't start.
    pub fn start<K, R>(
        &self,
        source: Source,
        kept: K,
        results: &TapeMux<R>,
    ) -> Result<RfRunning<K, R>, RfError<K::Error, R::Error>>
    where
        K: PieceStore + Send + 'static,
        K::Error: Send,
        R: PieceStore + Send + 'static,
        R::Sink: Send,
        R::Key: Send,
        R::Error: Send + Sync,
    {
        let def = self
            .wb
            .channel()
            .def()
            .ok_or(RfError::Settings("a channel's rate"))?;
        let chain = window::Chain {
            kept,
            analyser: Bursts::new(self, def, results).map_err(RfError::Results)?,
        };
        let wb = self.wb;
        let running = window::start(
            OWNER,
            self.wb.chain_options(),
            self.window,
            vec![chain],
            move |muxes| wb.record(muxes, &source),
        )
        .map_err(RfError::Run)?;
        Ok(RfRunning { running })
    }

    /// Record the receiver into memory as [Self::start] does and wait for the end.
    ///
    /// # Errors
    ///
    /// What [Self::start] and [RfRunning::join] fail with.
    pub fn run<K, R>(
        &self,
        source: Source,
        kept: K,
        results: &TapeMux<R>,
    ) -> Result<RfRun, RfError<K::Error, R::Error>>
    where
        K: PieceStore + Send + 'static,
        K::Error: Send,
        R: PieceStore + Send + 'static,
        R::Sink: Send,
        R::Key: Send,
        R::Error: Send + Sync,
    {
        self.start(source, kept, results)?.join()
    }
}

/// A capture of the receiver into memory, running ([RfCapture::start]).
pub struct RfRunning<K, R>
where
    K: PieceStore,
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    running: window::Running<Recorded, WidebandError<MemError>, K, Bursts<R>>,
}

impl<K, R> RfRunning<K, R>
where
    K: PieceStore,
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    /// The memory window: one chain, its pieces in memory and the watch of its mux.
    pub fn chains(&self) -> &[MemChain] {
        self.running.chains()
    }

    /// Whether the capture and the triage ended.
    pub fn is_finished(&self) -> bool {
        self.running.is_finished()
    }

    /// Wait for the end: what each channel pushed, what the window kept and every burst found. The results' handles
    /// end, so `results` may close after.
    ///
    /// # Errors
    ///
    /// The capture's, the triage's or the results' error.
    pub fn join(self) -> Result<RfRun, RfError<K::Error, R::Error>> {
        let (ran, chains) = self.running.join().map_err(RfError::Run)?;
        let [window::Chain { analyser, .. }] = <[_; 1]>::try_from(chains)
            .map_err(|_chains| RfError::Settings("a triage of the one chain"))?;
        let bursts = analyser.finish().map_err(RfError::Results)?;
        let [triaged] = <[Triaged; 1]>::try_from(ran.triaged)
            .map_err(|_triaged| RfError::Settings("a triage of the one chain"))?;
        Ok(RfRun {
            recorded: ran.recorded,
            capture_time: ran.capture_time,
            triaged,
            bursts,
        })
    }
}

/// What [RfCapture::run] did: what each channel pushed, what the window kept, and every burst the analyser found.
#[derive(Clone, Debug, PartialEq)]
pub struct RfRun {
    pub recorded: Recorded,
    /// [Ran::capture_time](example_capture::window::Ran::capture_time).
    pub capture_time: core::time::Duration,
    pub triaged: Triaged,
    pub bursts: Vec<Burst>,
}

/// A burst the analyser found on a channel: the span of the summary nodes that hold it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Burst {
    pub channel: u32,
    /// The first node's first tick, and the tick after the last node.
    pub start: i64,
    pub end: i64,
    /// The largest power `I² + Q²` of its nodes.
    pub power_max: f32,
}

/// The results' track `bursts` of channel `k` on `clock`.
pub fn bursts_spec(k: u32, clock: ClockNum) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(
            RESULTS_ROOT,
            &[
                Segment::Name("channel"),
                Segment::Index(k),
                Segment::Name("bursts"),
            ],
        ),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: "bursts",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef {
            ends: Ends::Spans,
            flags: false,
        }),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// The results' track `kept`, on `clock`.
pub fn kept_track(clock: ClockNum) -> TrackSpec<'static> {
    kept_spec(track_key(RESULTS_ROOT, &[Segment::Name("kept")]), clock)
}

/// A channel as the analyser reads and reports it.
struct Channel<R>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    k: u32,
    /// The id of its track `iq`.
    iq: TrackId,
    /// Its `bursts` in the results.
    bursts: EventsHandle<R>,
    /// A burst that lasts to the end of the nodes read so far.
    open: Option<Burst>,
}

/// The analyser of [RfCapture]: it detects energy in the summary nodes of each piece, keeps the pieces that hold a
/// burst and pushes its results.
pub struct Bursts<R>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    def: GridDef<'static>,
    power_min: f32,
    channels: Vec<Channel<R>>,
    kept: KeptLog<R>,
    found: Vec<Burst>,
    buf: Vec<u8>,
}

impl<R> Bursts<R>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    /// The analyser of `c`'s channels, whose series definition is `def`, its results' clock and tracks defined on
    /// `results`.
    ///
    /// # Errors
    ///
    /// A definition or handle `results` refused.
    pub fn new(
        c: &RfCapture,
        def: GridDef<'static>,
        results: &TapeMux<R>,
    ) -> Result<Self, MuxError<R::Error>> {
        let clock = results.define_clock(clock())?;
        let options = EventsOptions {
            latency: None,
            ..EventsOptions::default()
        };
        let mut channels = Vec::new();
        for k in 0..c.wb.channels {
            let track = results.define_track(bursts_spec(k, clock))?;
            channels.push(Channel {
                k,
                iq: channel_id(k),
                bursts: results.events(track, options)?,
                open: None,
            });
        }
        let kept = results.events(results.define_track(kept_track(clock))?, options)?;
        Ok(Self {
            def,
            power_min: c.power_min,
            channels,
            kept: KeptLog::new(kept),
            found: Vec::new(),
            buf: Vec::new(),
        })
    }

    /// Push the bursts still open, the `kept` items, and end the results' handles: every burst found, in the order
    /// found.
    ///
    /// # Errors
    ///
    /// A handle whose last chunk the mux refused.
    pub fn finish(mut self) -> Result<Vec<Burst>, MuxError<R::Error>> {
        for c in &mut self.channels {
            if let Some(b) = c.open.take() {
                push(&mut c.bursts, &b)?;
                self.found.push(b);
            }
        }
        for c in self.channels {
            c.bursts.finish()?;
        }
        self.kept.finish()?;
        Ok(self.found)
    }
}

/// Push burst `b` into its channel's `bursts`.
fn push<R>(handle: &mut EventsHandle<R>, b: &Burst) -> Result<(), MuxError<R::Error>>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    handle.push_all(&[Event {
        start: b.start,
        end: b.end,
        payload: &b.power_max.to_le_bytes(),
        ..Event::default()
    }])?;
    Ok(())
}

impl<R> Analyser for Bursts<R>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    type Error = LookError<WidebandError<MemError>, R::Error>;

    fn look(&mut self, _seq: u64, piece: &TapeReader<MemPiece>) -> Result<bool, Self::Error> {
        let mut hit = false;
        for c in &mut self.channels {
            // --8<-- [start:detect]
            // the level-0 nodes only, each 1 024 samples: never the samples themselves
            let nodes = summary_nodes(piece, &self.def, c.iq, 0, &mut self.buf)
                .map_err(LookError::Piece)?;
            for n in nodes {
                if n.power_max < self.power_min {
                    if let Some(b) = c.open.take() {
                        push(&mut c.bursts, &b)?;
                        self.found.push(b);
                    }
                    continue;
                }
                hit = true;
                let b = c.open.get_or_insert(Burst {
                    channel: c.k,
                    start: n.first_tick,
                    end: n.end_tick,
                    power_max: n.power_max,
                });
                b.end = n.end_tick;
                b.power_max = b.power_max.max(n.power_max);
            }
            // --8<-- [end:detect]
        }
        Ok(hit)
    }

    fn kept(&mut self, seq: u64, piece: &TapeReader<MemPiece>) -> Result<(), Self::Error> {
        self.kept
            .note(seq, piece)
            .map_err(|e| LookError::Piece(e.into()))
    }
}

/// Why [RfCapture::run] failed: `K` is the kept store's error, `R` the results' store's.
#[derive(Debug)]
#[non_exhaustive]
pub enum RfError<K, R> {
    /// A setting of [RfCapture] the capture can't be made with.
    Settings(&'static str),
    /// The results' mux refused a definition or a handle, or the last chunk of one.
    Results(MuxError<R>),
    /// The capture, the window or the analyser.
    Run(RunError<WidebandError<MemError>, K, LookError<WidebandError<MemError>, R>>),
}

impl<K: core::fmt::Debug, R: core::fmt::Debug> core::fmt::Display for RfError<K, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Settings(what) => write!(f, "the capture: {what}"),
            Self::Results(e) => write!(f, "the results: {e:?}"),
            Self::Run(e) => write!(f, "{e}"),
        }
    }
}

impl<K: core::fmt::Debug, R: core::fmt::Debug> core::error::Error for RfError<K, R> {}
