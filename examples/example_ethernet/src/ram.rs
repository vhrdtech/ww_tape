//! The analyser capturing the link into memory at line rate and keeping only the pieces around TCP resets.
//!
//! [RamCapture::run] records the link as [Link::record] does, into chains of pieces in memory ([example_capture::window]):
//! port `p` into chain `p % chains`, through a mux of its own. A chain's analyser, [Resets], reads each piece of its
//! chain as it closes, on a thread of its own. It decodes every frame of the piece and looks for a TCP
//! reset ([L4::is_reset]). A piece that holds one is kept, with [Window::before] pieces before it and
//! [Window::after] after it; the rest leave memory unwritten.
//!
//! The analyser's results go to a chain of their own, through `results`, a mux on any store:
//!
//! | Track | Item | Payload |
//! |---|---|---|
//! | `resets` of each receive queue | a TCP reset at its frame's tick, the frame's number in the relation `frame` | the frame's `ipv4` and `l4` items: [RESET_BYTES] |
//! | `kept` of each chain | a piece of the chain kept, from its first item's tick to its last one's ([KeptLog]) | the piece's sequence number, 8 bytes LE |
//!
//! A reset names its frame by its item number in the queue's `frames`. The frame lies in the kept piece of its chain
//! the analyser found it in, and a reader finds it by that number there.

use std::num::NonZeroU32;
use std::sync::Arc;

use rayon::prelude::*;
use shrink_wrap::prelude::*;
use ww_tape::payloads::Payloads;
use ww_tape::times::EventTimes;
use ww_tape::{
    BlockKind, ChainOptions, ClockNum, CutPolicy, Ends, EventsDef, Items, MemError, MemPiece,
    PieceStore, RelationDef, Segment, TapeReader, TrackId, TrackKind, TrackRepeat, TrackSpec,
    track_key,
};
use ww_tape_mux::{Event, EventsHandle, EventsOptions, MuxError, TapeMux};

use crate::{
    Captured, EthError, IPV4_BYTES, Ipv4, L4, L4_BYTES, Link, OWNER, Rx, clock, decode_frame,
    frame_relation, frames_id,
};
use example_capture::chain_options;
use example_capture::window::{
    self, Analyser, KeptLog, LookError, MemChain, RunError, Triaged, Window, kept_spec,
};

/// The root of the results' API: their track and clock keys start from it.
pub const RESULTS_ROOT: u64 = 0xe7_0002;
/// Bytes of an item of `resets`: the frame's `ipv4` and `l4` items.
pub const RESET_BYTES: usize = IPV4_BYTES + L4_BYTES;

// --8<-- [start:ram_capture]
/// A capture of the link into a window in memory, the pieces around each TCP reset kept.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RamCapture {
    pub link: Link,
    /// Chains in memory, each written by a mux and read by a triage of its own: 1 or 2, one per port.
    pub chains: u32,
    /// Bytes of a piece in memory at most: the unit the window keeps or drops.
    pub piece_bytes: u64,
    pub window: Window,
    /// Threads the triages of every chain share to decode the chunks of a piece in parallel; None: 2 a chain
    /// ([Self::triage_threads_used]). Nothing keeps these threads off the capture's cores: a pool larger than the
    /// cores the capture leaves free takes time from the capture.
    pub triage_threads: Option<NonZeroU32>,
}
// --8<-- [end:ram_capture]

impl RamCapture {
    /// A second of the link at its line rate over 4 receive queues a port, a chain per port, pieces of 64 MiB, a
    /// window of 4 GiB a chain, the piece before and the piece after a reset kept with it.
    pub const fn full() -> Self {
        Self {
            link: Link::full().queues(4),
            chains: 2,
            piece_bytes: 64 << 20,
            window: Window {
                bytes_max: 4 << 30,
                before: 1,
                after: 1,
            },
            triage_threads: None,
        }
    }

    /// The threads of the triage pool: [Self::triage_threads], or 2 a chain.
    ///
    /// The bench's sweep of the pool's size sets the default (the book's page on this case shows it): one thread
    /// a chain keeps pace on the bench machine and 2 keep it with margin. So the default is the smallest pool with
    /// margin: 2 threads a chain, whatever the cores of the machine.
    pub fn triage_threads_used(&self) -> usize {
        self.triage_threads
            .map_or(self.chains as usize * 2, |n| n.get() as usize)
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

    /// Start recording the link into memory, the pieces of chain `c` around each reset kept in `kept[c]` and the
    /// analysers' results pushed through `results`, as the [module docs](self) say. The capture runs on threads of
    /// its own; the [RamRunning] it gives hands out the memory window while it runs.
    ///
    /// # Errors
    ///
    /// Not a kept store per chain, a results' definition or handle refused, or a mux or thread that didn't start.
    pub fn start<K, R>(
        &self,
        kept: Vec<K>,
        results: &TapeMux<R>,
    ) -> Result<RamRunning<K, R>, RamError<K::Error, R::Error>>
    where
        K: PieceStore + Send + 'static,
        K::Error: Send,
        R: PieceStore + Send + 'static,
        R::Sink: Send,
        R::Key: Send,
        R::Error: Send + Sync,
    {
        if !(1..=2).contains(&self.chains) || kept.len() != self.chains as usize {
            return Err(RamError::Chains);
        }
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(self.triage_threads_used())
                .thread_name(|i| format!("triage-{i}"))
                .build()
                .map_err(RamError::Pool)?,
        );
        let clock = results.define_clock(clock()).map_err(RamError::Results)?;
        let chains = kept
            .into_iter()
            .zip(0..)
            .map(|(kept, chain)| {
                Ok(window::Chain {
                    kept,
                    analyser: Resets::new(&self.link, (chain, self.chains), clock, &pool, results)?,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(RamError::Results)?;
        let link = self.link;
        let running = window::start(
            OWNER,
            self.chain_options(),
            self.window,
            chains,
            move |muxes| link.record(muxes),
        )
        .map_err(RamError::Run)?;
        Ok(RamRunning { running })
    }

    /// Record the link into memory as [Self::start] does and wait for the end.
    ///
    /// # Errors
    ///
    /// What [Self::start] and [RamRunning::join] fail with.
    pub fn run<K, R>(
        &self,
        kept: Vec<K>,
        results: &TapeMux<R>,
    ) -> Result<RamRun, RamError<K::Error, R::Error>>
    where
        K: PieceStore + Send + 'static,
        K::Error: Send,
        R: PieceStore + Send + 'static,
        R::Sink: Send,
        R::Key: Send,
        R::Error: Send + Sync,
    {
        self.start(kept, results)?.join()
    }
}

/// A capture of the link into memory, running ([RamCapture::start]).
pub struct RamRunning<K, R>
where
    K: PieceStore,
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    running: window::Running<Captured, EthError<MemError>, K, Resets<R>>,
}

impl<K, R> RamRunning<K, R>
where
    K: PieceStore,
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    /// The memory window, a chain per port ([RamCapture::chains]): its pieces in memory and the watch of its mux.
    pub fn chains(&self) -> &[MemChain] {
        self.running.chains()
    }

    /// Whether the capture and every triage ended.
    pub fn is_finished(&self) -> bool {
        self.running.is_finished()
    }

    /// Wait for the end: what the ports sent, what each chain's window kept and every reset found. The results'
    /// handles end, so `results` may close after.
    ///
    /// # Errors
    ///
    /// The capture's, a triage's or the results' error.
    pub fn join(self) -> Result<RamRun, RamError<K::Error, R::Error>> {
        let (ran, chains) = self.running.join().map_err(RamError::Run)?;
        let mut resets = Vec::new();
        for c in chains {
            resets.extend(c.analyser.finish().map_err(RamError::Results)?);
        }
        Ok(RamRun {
            captured: ran.recorded,
            capture_time: ran.capture_time,
            triaged: ran.triaged,
            resets,
        })
    }
}

/// What [RamCapture::run] did: what the ports sent, what the window of each chain kept, and every reset the analysers
/// found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RamRun {
    pub captured: Captured,
    /// [Ran::capture_time](example_capture::window::Ran::capture_time).
    pub capture_time: core::time::Duration,
    /// A [Triaged] per chain.
    pub triaged: Vec<Triaged>,
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
    /// The chain of the capture that holds it, and the piece of that chain.
    pub chain: u32,
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

    /// The reset of queue `rx` from its tick, its frame and its payload in `resets`; it doesn't say its chain or its
    /// piece, which are 0.
    pub fn from_bytes(rx: Rx, tick: i64, frame: u64, b: &[u8]) -> Option<Self> {
        let (ipv4, l4) = b.split_at_checked(IPV4_BYTES)?;
        Some(Self {
            tick,
            rx,
            frame,
            ipv4: Ipv4::from_bytes(ipv4)?,
            l4: L4::from_bytes(l4)?,
            chain: 0,
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
        defined_twice: false,
    }
}

/// The results' track `kept` of chain `chain`, on `clock`.
pub fn kept_track(chain: u32, clock: ClockNum) -> TrackSpec<'static> {
    kept_spec(
        track_key(
            RESULTS_ROOT,
            &[
                Segment::Name("chain"),
                Segment::Index(chain),
                Segment::Name("kept"),
            ],
        ),
        clock,
    )
}

/// A receive queue as the analyser reads and reports it.
struct Queue<R>
where
    R: PieceStore + Send + 'static,
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

/// The analyser of a chain of [RamCapture]: it finds the TCP resets among the frames of each piece, keeps the pieces
/// that hold one and pushes its results.
pub struct Resets<R>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    chain: u32,
    queues: Vec<Queue<R>>,
    kept: KeptLog<R>,
    found: Vec<Reset>,
    pool: Arc<rayon::ThreadPool>,
}

impl<R> Resets<R>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    /// The analyser of chain `chain` of `chains`, as `(chain, chains)`: the receive queues of `link`'s ports `p` with
    /// `p % chains == chain`, its results' tracks defined on `results` on `clock`. The chunks of a piece are decoded on
    /// `pool`.
    ///
    /// # Errors
    ///
    /// A definition or handle `results` refused.
    pub fn new(
        link: &Link,
        (chain, chains): (u32, u32),
        clock: ClockNum,
        pool: &Arc<rayon::ThreadPool>,
        results: &TapeMux<R>,
    ) -> Result<Self, MuxError<R::Error>> {
        let options = EventsOptions {
            latency: None,
            ..EventsOptions::default()
        };
        let mut queues = Vec::new();
        for rx in link.rxs().filter(|rx| rx.port % chains.max(1) == chain) {
            let relation = [frame_relation(rx)];
            let track = results.define_track(resets_spec(rx, clock, &relation))?;
            queues.push(Queue {
                rx,
                frames: frames_id(rx),
                resets: results.events(track, options)?,
            });
        }
        let kept = results.events(results.define_track(kept_track(chain, clock))?, options)?;
        Ok(Self {
            chain,
            queues,
            kept: KeptLog::new(kept),
            found: Vec::new(),
            pool: Arc::clone(pool),
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
/// The resets among the frames of queue `frames` (its id) in piece `seq` of chain `chain`: each frame decoded, its
/// TCP flags tested.
fn resets_of(
    piece: &TapeReader<MemPiece>,
    (chain, seq): (u32, u64),
    rx: Rx,
    frames: TrackId,
) -> Result<Vec<Reset>, EthError<MemError>> {
    let entries: Vec<_> = piece
        .track_entries(0, frames)?
        .into_iter()
        .filter(|entry| entry.kind == BlockKind::Events)
        .collect();
    // the chunks of a piece are independent: each worker decodes whole chunks, the resets stay in chunk order
    let per_chunk = entries
        .par_iter()
        .map_init(Vec::new, |buf, entry| {
            let block = piece.read_block(0, entry, buf)?;
            let chunk = block.chunk().ok_or(EthError::Read("a chunk"))?;
            let times = EventTimes::read(&chunk)?;
            let payloads = Payloads::read(&chunk)?.ok_or(EthError::Read("payloads"))?;
            let mut out = Vec::new();
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
                        chain,
                        piece: seq,
                    });
                }
            }
            Ok::<_, EthError<MemError>>(out)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(per_chunk.concat())
}
// --8<-- [end:look]

impl<R> Analyser for Resets<R>
where
    R: PieceStore + Send + 'static,
    R::Sink: Send,
    R::Key: Send,
    R::Error: Send + Sync,
{
    type Error = LookError<EthError<MemError>, R::Error>;

    fn look(&mut self, seq: u64, piece: &TapeReader<MemPiece>) -> Result<bool, Self::Error> {
        let mut hit = false;
        for q in &mut self.queues {
            let found = self
                .pool
                .install(|| resets_of(piece, (self.chain, seq), q.rx, q.frames))
                .map_err(LookError::Piece)?;
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

    fn kept(&mut self, seq: u64, piece: &TapeReader<MemPiece>) -> Result<(), Self::Error> {
        self.kept
            .note(seq, piece)
            .map_err(|e| LookError::Piece(e.into()))
    }
}

/// Why [RamCapture::run] failed: `K` is the kept store's error, `R` the results' store's.
#[derive(Debug)]
#[non_exhaustive]
pub enum RamError<K, R> {
    /// [RamCapture::chains] is not 1 or 2, or not the number of kept stores.
    Chains,
    /// The results' mux refused a definition or a handle, or the last chunk of one.
    Results(MuxError<R>),
    /// The triage's thread pool didn't start.
    Pool(rayon::ThreadPoolBuildError),
    /// The capture, the window or the analyser.
    Run(RunError<EthError<MemError>, K, LookError<EthError<MemError>, R>>),
}

impl<K: core::fmt::Debug, R: core::fmt::Debug> core::fmt::Display for RamError<K, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Chains => f.write_str("1 or 2 chains, a kept store each"),
            Self::Results(e) => write!(f, "the results: {e:?}"),
            Self::Pool(e) => write!(f, "the triage's threads: {e}"),
            Self::Run(e) => write!(f, "{e}"),
        }
    }
}

impl<K: core::fmt::Debug, R: core::fmt::Debug> core::error::Error for RamError<K, R> {}
