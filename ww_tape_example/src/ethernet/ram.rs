//! The analyser capturing the link into memory at line rate and keeping only the pieces around TCP resets.
//!
//! [RamCapture::run] records the link as [Link::record] does, into a chain of pieces in memory ([crate::window]).
//! Its analyser, [Resets], reads each piece as it closes. It decodes every frame of the piece and looks for a TCP
//! reset ([L4::is_reset]). A piece that holds one is kept, with [Window::before] pieces before it and
//! [Window::after] after it; the rest leave memory unwritten.
//!
//! The analyser's results go to a chain of their own, through `results`, a mux on any store:
//!
//! | Track | Item | Payload |
//! |---|---|---|
//! | `resets` of each receive queue | a TCP reset at its frame's tick, the frame's number in the relation `frame` | the frame's `ipv4` and `l4` items: [RESET_BYTES] |
//! | `kept` | a piece kept, from its first item's tick to its last one's ([KeptLog]) | the piece's sequence number, 8 bytes LE |
//!
//! A reset names its frame by its item number in the queue's `frames`. The frame lies in the kept piece the analyser
//! found it in, and a reader finds it by that number there.

use shrink_wrap::prelude::*;
use ww_tape::payloads::Payloads;
use ww_tape::times::EventTimes;
use ww_tape::{
    BlockKind, ChainOptions, ClockNum, CutPolicy, Ends, EventsDef, Items, MemError, MemTape,
    RelationDef, Segment, TapeReader, Tapes, TrackId, TrackKind, TrackRepeat, TrackSpec, track_key,
};
use ww_tape_mux::{Event, EventsHandle, EventsOptions, MuxError, TapeMux};

use super::{
    EthError, IPV4_BYTES, Ipv4, L4, L4_BYTES, Link, OWNER, Rx, chain_options, clock, decode_frame,
    frame_relation, frames_id,
};
use crate::window::{self, Analyser, KeptLog, LookError, RunError, Triaged, Window, kept_spec};

/// The root of the results' API: their track and clock keys start from it.
pub const RESULTS_ROOT: u64 = 0xe7_0002;
/// Bytes of an item of `resets`: the frame's `ipv4` and `l4` items.
pub const RESET_BYTES: usize = IPV4_BYTES + L4_BYTES;

// --8<-- [start:ram_capture]
/// A capture of the link into a window in memory, the pieces around each TCP reset kept.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RamCapture {
    pub link: Link,
    /// Bytes of a piece in memory at most: the unit the window keeps or drops.
    pub piece_bytes: u64,
    pub window: Window,
}
// --8<-- [end:ram_capture]

impl RamCapture {
    /// A second of the link at its line rate over 4 receive queues a port, pieces of 64 MiB, a window of 8 GiB, the
    /// piece before and the piece after a reset kept with it.
    pub const fn full() -> Self {
        Self {
            link: Link::full().queues(4),
            piece_bytes: 64 << 20,
            window: Window {
                bytes_max: 8 << 30,
                before: 1,
                after: 1,
            },
        }
    }

    /// The chain options of the capture in memory: [chain_options] cut at [Self::piece_bytes].
    pub fn chain_options(&self) -> ChainOptions {
        ChainOptions {
            cut: CutPolicy {
                bytes: exact_units::Bytes::new(self.piece_bytes),
                ..CutPolicy::default()
            },
            ..chain_options()
        }
    }

    /// Record the link into memory, keep the pieces around each reset in `kept` and the analyser's results through
    /// `results`, as the [module docs](self) say.
    ///
    /// # Errors
    ///
    /// The capture's, the triage's or the results' error.
    pub fn run<K, R>(
        &self,
        kept: &mut K,
        results: &TapeMux<R>,
    ) -> Result<RamRun, RamError<K::Error, R::Error>>
    where
        K: Tapes + Send,
        K::Error: Send,
        R: Tapes + Send + 'static,
        R::Sink: Send,
        R::Key: Send,
        R::Error: Send + Sync,
    {
        let mut analyser = Resets::new(&self.link, results).map_err(RamError::Results)?;
        let ran = window::run(
            OWNER,
            self.chain_options(),
            self.window,
            kept,
            &mut analyser,
            |mux| self.link.record(&[mux]),
        )
        .map_err(RamError::Run)?;
        let resets = analyser.finish().map_err(RamError::Results)?;
        Ok(RamRun {
            captured: ran.recorded,
            capture_time: ran.capture_time,
            triaged: ran.triaged,
            resets,
        })
    }
}

/// What [RamCapture::run] did: what the ports sent, what the window kept, and every reset the analyser found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RamRun {
    pub captured: super::Captured,
    /// [Ran::capture_time](crate::window::Ran::capture_time).
    pub capture_time: core::time::Duration,
    pub triaged: Triaged,
    pub resets: Vec<Reset>,
}

/// A TCP reset the analyser found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Reset {
    /// Its frame's tick, its receive queue and its item number in the queue's `frames`.
    pub tick: i64,
    pub rx: Rx,
    pub frame: u64,
    pub ipv4: Ipv4,
    pub l4: L4,
    /// The piece of the capture that holds it.
    pub piece: u64,
}

impl Reset {
    /// Its payload in `resets`.
    pub fn to_bytes(&self) -> [u8; RESET_BYTES] {
        let mut b = [0; RESET_BYTES];
        let (ip, l4) = b.split_at_mut(IPV4_BYTES);
        ip.copy_from_slice(&self.ipv4.to_bytes());
        l4.copy_from_slice(&self.l4.to_bytes());
        b
    }

    /// The reset of queue `rx` from its tick, its frame and its payload in `resets`; it doesn't say its piece,
    /// which is 0.
    pub fn from_bytes(rx: Rx, tick: i64, frame: u64, b: &[u8]) -> Option<Self> {
        let (ipv4, l4) = b.split_at_checked(IPV4_BYTES)?;
        Some(Self {
            tick,
            rx,
            frame,
            ipv4: Ipv4::from_bytes(ipv4)?,
            l4: L4::from_bytes(l4)?,
            piece: 0,
        })
    }
}

/// The key of the results' track `name`; of a receive queue's with `rx`.
pub fn results_key(rx: Option<Rx>, name: &str) -> ww_tape::TrackKey {
    match rx {
        None => track_key(RESULTS_ROOT, &[Segment::Name(name)]),
        Some(rx) => track_key(
            RESULTS_ROOT,
            &[
                Segment::Name("port"),
                Segment::Index(rx.port),
                Segment::Name("queue"),
                Segment::Index(rx.queue),
                Segment::Name(name),
            ],
        ),
    }
}

/// The results' track `resets` of receive queue `rx` on `clock`, its items related to their frames by `relations`
/// ([frame_relation]).
pub fn resets_spec<'r>(rx: Rx, clock: ClockNum, relations: &'r [RelationDef<'r>]) -> TrackSpec<'r> {
    TrackSpec {
        key: results_key(Some(rx), "resets"),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: "resets",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef {
            ends: Ends::Instants,
            flags: false,
        }),
        relations: RefVec::Slice { slice: relations },
        owner: [0; 32],
    }
}

/// The results' track `kept`, on `clock`.
pub fn kept_track(clock: ClockNum) -> TrackSpec<'static> {
    kept_spec(RESULTS_ROOT, clock)
}

/// A receive queue as the analyser reads and reports it.
struct Queue<R>
where
    R: Tapes + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    rx: Rx,
    /// The id of its `frames`.
    frames: TrackId,
    /// Its `resets` in the results.
    resets: EventsHandle<R>,
}

/// The analyser of [RamCapture]: it finds the TCP resets among the frames of each piece, keeps the pieces that hold
/// one and pushes its results.
pub struct Resets<R>
where
    R: Tapes + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    queues: Vec<Queue<R>>,
    kept: KeptLog<R>,
    found: Vec<Reset>,
    buf: Vec<u8>,
}

impl<R> Resets<R>
where
    R: Tapes + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    /// The analyser of `link`'s receive queues, its results' clock and tracks defined on `results`.
    ///
    /// # Errors
    ///
    /// A definition or handle `results` refused.
    pub fn new(link: &Link, results: &TapeMux<R>) -> Result<Self, MuxError<R::Error>> {
        let clock = results.define_clock(clock())?;
        let options = EventsOptions {
            latency: None,
            ..EventsOptions::default()
        };
        let mut queues = Vec::new();
        for rx in link.rxs() {
            let relation = [frame_relation(rx)];
            let track = results.define_track(resets_spec(rx, clock, &relation))?;
            queues.push(Queue {
                rx,
                frames: frames_id(rx),
                resets: results.events(track, options)?,
            });
        }
        let kept = results.events(results.define_track(kept_track(clock))?, options)?;
        Ok(Self {
            queues,
            kept: KeptLog::new(kept),
            found: Vec::new(),
            buf: Vec::new(),
        })
    }

    /// Push the `kept` items and end the results' handles: every reset found, in the order found.
    ///
    /// # Errors
    ///
    /// A handle whose last chunk the mux refused.
    pub fn finish(self) -> Result<Vec<Reset>, MuxError<R::Error>> {
        for q in self.queues {
            q.resets.finish()?;
        }
        self.kept.finish()?;
        Ok(self.found)
    }
}

// --8<-- [start:look]
/// The resets among the frames of queue `frames` (its id) in piece `seq`: each frame decoded, its TCP flags tested.
fn resets_of(
    piece: &TapeReader<MemTape>,
    seq: u64,
    rx: Rx,
    frames: TrackId,
    buf: &mut Vec<u8>,
) -> Result<Vec<Reset>, EthError<MemError>> {
    let mut out = Vec::new();
    for entry in piece.track_entries(0, frames)? {
        if entry.kind != BlockKind::Events {
            continue;
        }
        let block = piece.read_block(0, &entry, buf)?;
        let chunk = block.chunk().ok_or(EthError::Read("a chunk"))?;
        let times = EventTimes::read(&chunk)?;
        let payloads = Payloads::read(&chunk)?.ok_or(EthError::Read("payloads"))?;
        for j in 0..times.len() {
            let frame = payloads.get(j).ok_or(EthError::Read("a frame"))?;
            if let Some((ipv4, Some(l4))) = decode_frame(frame)
                && l4.is_reset()
            {
                out.push(Reset {
                    tick: times.start(j),
                    rx,
                    frame: entry.first_item.get() + j as u64,
                    ipv4,
                    l4,
                    piece: seq,
                });
            }
        }
    }
    Ok(out)
}
// --8<-- [end:look]

impl<R> Analyser for Resets<R>
where
    R: Tapes + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    type Error = LookError<EthError<MemError>, R::Error>;

    fn look(&mut self, seq: u64, piece: &TapeReader<MemTape>) -> Result<bool, Self::Error> {
        let mut hit = false;
        for q in &mut self.queues {
            let found =
                resets_of(piece, seq, q.rx, q.frames, &mut self.buf).map_err(LookError::Piece)?;
            let payloads: Vec<[u8; RESET_BYTES]> = found.iter().map(Reset::to_bytes).collect();
            let frames: Vec<u64> = found.iter().map(|r| r.frame).collect();
            let events: Vec<Event<'_>> = found
                .iter()
                .zip(&payloads)
                .zip(frames.chunks(1))
                .map(|((r, payload), frame)| Event {
                    start: r.tick,
                    end: r.tick,
                    flags: 0,
                    payload,
                    values: frame,
                })
                .collect();
            q.resets.push_all(&events)?;
            hit |= !found.is_empty();
            self.found.extend(found);
        }
        Ok(hit)
    }

    fn kept(&mut self, seq: u64, piece: &TapeReader<MemTape>) -> Result<(), Self::Error> {
        self.kept
            .note(seq, piece)
            .map_err(|e| LookError::Piece(e.into()))
    }
}

/// Why [RamCapture::run] failed: `K` is the kept store's error, `R` the results' store's.
#[derive(Debug)]
#[non_exhaustive]
pub enum RamError<K, R> {
    /// The results' mux refused a definition or a handle, or the last chunk of one.
    Results(MuxError<R>),
    /// The capture, the window or the analyser.
    Run(RunError<EthError<MemError>, K, LookError<EthError<MemError>, R>>),
}

impl<K: core::fmt::Debug, R: core::fmt::Debug> core::fmt::Display for RamError<K, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Results(e) => write!(f, "the results: {e:?}"),
            Self::Run(e) => write!(f, "{e}"),
        }
    }
}

impl<K: core::fmt::Debug, R: core::fmt::Debug> core::error::Error for RamError<K, R> {}
