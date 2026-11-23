//! A capture into memory at line rate: the memory window, an analyser that reads each piece as it closes, and only the
//! pieces worth keeping copied to a store of their own.
//!
//! The memory window is a chain of pieces kept in memory: the tape of the capture held in RAM, its newest stretch. A
//! [TapeMux] writes it into a [MemPieces] that holds each piece apart ([PieceTapes::Each](ww_tape::PieceTapes)), cut by
//! size. Each piece holds the same tracks, blocks and summaries a piece in a file holds, and the tracks run across the
//! pieces. A mux has one writing thread, so [start] spreads a capture faster than one thread copies over several
//! chains, each with a mux of its own.
//!
//! [triage] runs beside each chain in the same process, on a thread of its own. It waits on the mux's
//! [ProgressWatch], opens each piece once it closes ([TapeReader] over the [MemPiece], no copy) and asks the [Analyser]
//! whether to keep it. A kept piece is copied whole to the kept store, such as a directory of piece files, where it
//! reads alone. Every other piece leaves memory ([MemPieces::remove]). [start] returns a [Running] capture that hands
//! out each chain ([MemChain]), so a viewer follows the window while the capture runs ([Follower]).
//!
//! [Window::before] and [Window::after] keep the pieces around a kept one too, so the analyser's verdict on one piece
//! keeps what led up to it and what followed. The pieces before wait in memory, already read, until no later verdict
//! can keep them. When the analyser falls behind and the window holds more than [Window::bytes_max], the oldest
//! pieces go unread: [Triaged::overrun] counts them, as a capture card counts the frames it drops.
//!
//! ```
//! use example_capture::window::{Analyser, Window, kept_spec, triage};
//! use ww_tape::{ChainOptions, CutPolicy, MemPieces, OwnerId, PieceTapes, Segment, track_key};
//! use ww_tape_mux::{Event, EventsOptions, MuxOptions, TapeMux};
//!
//! // keeps every third piece
//! struct Third;
//! impl Analyser for Third {
//!     type Error = std::convert::Infallible;
//!     fn look(&mut self, seq: u64, _: &ww_tape::TapeReader<ww_tape::MemPiece>) -> Result<bool, Self::Error> {
//!         Ok(seq % 3 == 0)
//!     }
//! }
//!
//! let ram = MemPieces::new();
//! let options = ChainOptions {
//!     cut: CutPolicy { bytes: exact_units::Bytes::new(16 << 10), ..CutPolicy::default() },
//!     tapes: PieceTapes::Each,
//!     ..example_capture::chain_options()
//! };
//! let mux = TapeMux::create(ram.clone(), OwnerId([0xe7; 32]), 0, &[], options, MuxOptions::default())?;
//! let watch = mux.watch();
//! let clock = mux.define_clock(example_capture::rf::clock())?;
//! let track = mux.define_track(kept_spec(track_key(1, &[Segment::Name("ticks")]), clock))?;
//! let mut ticks = mux.events(track, EventsOptions { chunk_items: 256, latency: None, ..EventsOptions::default() })?;
//! let mut kept = MemPieces::new();
//! let triaged = std::thread::scope(|s| {
//!     let t = s.spawn(|| triage(&ram, &watch, Window::default(), &mut kept, &mut Third));
//!     for k in 0..20_000 {
//!         let event = Event { start: k, end: k + 1, payload: &(k * 7919).to_le_bytes(), ..Event::default() };
//!         ticks.push_all(&[event]).expect("a push");
//!     }
//!     ticks.finish().expect("the last chunk");
//!     mux.close().expect("the close");
//!     t.join().expect("the triage")
//! })?;
//! assert!(triaged.pieces > 3, "several pieces: {triaged:?}");
//! assert_eq!(triaged.kept, (0..triaged.pieces).filter(|s| s % 3 == 0).collect::<Vec<_>>());
//! assert!(ram.keys()?.is_empty(), "every piece left memory");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::VecDeque;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use shrink_wrap::prelude::*;
use ww_tape::reader::ReadError;
use ww_tape::{
    ByteSource, ChainOptions, ClockNum, Ends, EventsDef, Items, MemError, MemPiece, MemPieces,
    OwnerId, PieceStore, PieceTapes, TapeReader, TapeSink, TrackKey, TrackKind, TrackRepeat,
    TrackSpec,
};
use ww_tape_mux::{Event, EventsHandle, MuxError, MuxOptions, ProgressWatch, TapeMux};

/// Bytes [triage] copies from memory to the kept store in one write.
pub const COPY_BYTES: usize = 4 << 20;

/// What a window of a capture in memory keeps, and how much memory it may take.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Bytes the pieces of a chain in memory take at most, the piece being written included. Past it the oldest pieces leave
    /// memory: first those already read, then those not yet read ([Triaged::overrun]).
    pub bytes_max: u64,
    /// Pieces before a piece the analyser keeps that are kept with it.
    pub before: u32,
    /// Pieces after a piece the analyser keeps that are kept with it.
    pub after: u32,
}

impl Default for Window {
    /// 4 GiB of memory; no piece around a kept one.
    fn default() -> Self {
        Self {
            bytes_max: 4 << 30,
            before: 0,
            after: 0,
        }
    }
}

/// What reads the pieces of a capture in memory and decides which to keep.
pub trait Analyser {
    type Error: core::fmt::Debug;

    /// Read piece `seq` of the capture, closed: whether to keep it.
    fn look(&mut self, seq: u64, piece: &TapeReader<MemPiece>) -> Result<bool, Self::Error>;

    /// Piece `seq` reached the kept store; `piece` reads it. Called in the order of `seq`.
    fn kept(&mut self, _seq: u64, _piece: &TapeReader<MemPiece>) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<A: Analyser + ?Sized> Analyser for &mut A {
    type Error = A::Error;

    fn look(&mut self, seq: u64, piece: &TapeReader<MemPiece>) -> Result<bool, A::Error> {
        (**self).look(seq, piece)
    }

    fn kept(&mut self, seq: u64, piece: &TapeReader<MemPiece>) -> Result<(), A::Error> {
        (**self).kept(seq, piece)
    }
}

/// What [triage] did with the pieces of a capture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Triaged {
    /// Pieces the capture closed, and their bytes.
    pub pieces: u64,
    pub piece_bytes: u64,
    /// Pieces the analyser read.
    pub looked: u64,
    /// Pieces copied to the kept store, in order, and their bytes.
    pub kept: Vec<u64>,
    pub kept_bytes: u64,
    /// Pieces that left memory unread, since the window was full.
    pub overrun: Vec<u64>,
    /// The most bytes the pieces in memory took at one time.
    pub held_max: u64,
    /// Time spent reading pieces ([Analyser::look]) and copying the kept ones.
    pub looking: Duration,
    pub copying: Duration,
}

/// Run the window over the capture that `watch`'s mux writes into `ram`, each piece held apart, until the mux ends.
///
/// `analyser` reads every piece as it closes. The kept ones go to `kept` under their own sequence numbers, and every
/// piece leaves memory once no verdict can keep it.
///
/// # Errors
///
/// A piece that doesn't read or copy, a kept store that refused a piece, or the analyser's error.
pub fn triage<K: PieceStore, A: Analyser>(
    ram: &MemPieces,
    watch: &ProgressWatch,
    window: Window,
    kept: &mut K,
    mut analyser: A,
) -> Result<Triaged, WindowError<K::Error, A::Error>> {
    let mut t = Run {
        out: Triaged::default(),
        window,
        read: VecDeque::new(),
        keep_through: None,
        buf: Vec::new(),
    };
    let mut version = 0;
    let mut next = 0;
    loop {
        let progress = watch.wait_past(version);
        version = progress.version;
        while let Some(tape) = ram.closed(&next).map_err(WindowError::Ram)? {
            t.out.pieces += 1;
            t.out.piece_bytes += tape.size().map_err(WindowError::Ram)?;
            if t.make_room(ram, next)? {
                t.piece(ram, next, &tape, kept, &mut analyser)?;
            }
            next += 1;
        }
        if progress.ended {
            break;
        }
    }
    for seq in core::mem::take(&mut t.read) {
        ram.remove(&seq).map_err(WindowError::Ram)?;
    }
    Ok(t.out)
}

/// The state of a [triage].
struct Run {
    out: Triaged,
    window: Window,
    /// Pieces read and not kept, still in memory, oldest first: [Window::before] of them at most.
    read: VecDeque<u64>,
    /// Every piece up to this one is kept: [Window::after] past the last piece the analyser kept.
    keep_through: Option<u64>,
    buf: Vec<u8>,
}

impl Run {
    /// Bring memory under the window's bytes before piece `seq` is read: the pieces read leave first, oldest first,
    /// then the pieces not yet read. Whether `seq` is still there to read.
    fn make_room<K, A>(&mut self, ram: &MemPieces, seq: u64) -> Result<bool, WindowError<K, A>> {
        let mut held = held(ram)?;
        self.out.held_max = self.out.held_max.max(held);
        while held > self.window.bytes_max {
            let oldest = match self.read.pop_front() {
                Some(read) => read,
                None => match ram.keys().map_err(WindowError::Ram)?.first() {
                    Some(&first)
                        if first <= seq
                            && ram.closed(&first).map_err(WindowError::Ram)?.is_some() =>
                    {
                        self.out.overrun.push(first);
                        first
                    }
                    _ => break,
                },
            };
            ram.remove(&oldest).map_err(WindowError::Ram)?;
            held = self::held(ram)?;
            if oldest == seq {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Read piece `seq` and keep it, with those around it, or hold it in memory for a later verdict.
    fn piece<K: PieceStore, A: Analyser>(
        &mut self,
        ram: &MemPieces,
        seq: u64,
        tape: &MemPiece,
        kept: &mut K,
        analyser: &mut A,
    ) -> Result<(), WindowError<K::Error, A::Error>> {
        let reader = TapeReader::open(tape.clone()).map_err(WindowError::Read)?;
        let start = Instant::now();
        let hit = analyser.look(seq, &reader).map_err(WindowError::Analyser)?;
        self.out.looking += start.elapsed();
        self.out.looked += 1;
        if hit {
            for before in core::mem::take(&mut self.read) {
                let tape = ram
                    .closed(&before)
                    .map_err(WindowError::Ram)?
                    .ok_or(WindowError::Gone(before))?;
                let r = TapeReader::open(tape.clone()).map_err(WindowError::Read)?;
                self.keep(ram, before, &tape, &r, kept, analyser)?;
            }
            self.keep_through = Some(seq + u64::from(self.window.after));
        }
        if self.keep_through.is_some_and(|through| seq <= through) {
            return self.keep(ram, seq, tape, &reader, kept, analyser);
        }
        self.read.push_back(seq);
        while self.read.len() > self.window.before as usize {
            if let Some(old) = self.read.pop_front() {
                ram.remove(&old).map_err(WindowError::Ram)?;
            }
        }
        Ok(())
    }

    /// Copy piece `seq` to the kept store, tell the analyser and take it out of memory.
    fn keep<K: PieceStore, A: Analyser>(
        &mut self,
        ram: &MemPieces,
        seq: u64,
        tape: &MemPiece,
        reader: &TapeReader<MemPiece>,
        kept: &mut K,
        analyser: &mut A,
    ) -> Result<(), WindowError<K::Error, A::Error>> {
        let start = Instant::now();
        let key = kept.key_of(seq);
        let mut sink = kept.create(&key).map_err(WindowError::Kept)?;
        let (mut at, end) = (
            tape.start().map_err(WindowError::Ram)?,
            tape.size().map_err(WindowError::Ram)?,
        );
        while at < end {
            let n = usize::try_from(end - at)
                .unwrap_or(COPY_BYTES)
                .min(COPY_BYTES);
            self.buf.resize(n, 0);
            tape.read_at(at, &mut self.buf).map_err(WindowError::Ram)?;
            sink.write_all(&self.buf).map_err(WindowError::Kept)?;
            at += n as u64;
        }
        sink.sync().map_err(WindowError::Kept)?;
        sink.close().map_err(WindowError::Kept)?;
        self.out.copying += start.elapsed();
        self.out.kept.push(seq);
        self.out.kept_bytes += end;
        analyser.kept(seq, reader).map_err(WindowError::Analyser)?;
        ram.remove(&seq).map_err(WindowError::Ram)?;
        Ok(())
    }
}

/// A chain of a capture in memory as [start] reads it: the store its kept pieces go to and its analyser.
#[derive(Debug)]
pub struct Chain<K, A> {
    pub kept: K,
    pub analyser: A,
}

/// A chain of the memory window as a reader beside the triage sees it: its pieces in memory and the watch of the mux
/// that writes them.
///
/// The pieces hold the same tracks, blocks and summaries a piece in a file holds. A closed piece reads alone
/// ([MemPieces::closed], [TapeReader::open]); the piece being written has no close yet. The triage removes each piece
/// once no verdict can keep it, so a piece a reader looks for may be gone. A reader that holds a [MemPiece] keeps its
/// bytes in memory until it drops it, past [Window::bytes_max].
#[derive(Clone)]
pub struct MemChain {
    pub pieces: MemPieces,
    /// Wakes a reader on each change of the chain: a chunk written, a piece cut, the end.
    pub watch: ProgressWatch,
}

/// A piece a chain of the memory window began, as a [Follower] sees it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Cut {
    /// The chain, by its place in the chains given to [start].
    pub chain: usize,
    /// The sequence number of the piece begun: the pieces before it closed.
    pub piece: u64,
    /// Pieces of the chain in memory, the one begun included.
    pub held: usize,
}

/// Follows the chains of a memory window from another thread, as a viewer does: which pieces they begin.
#[derive(Clone, Debug, Default)]
pub struct Follower {
    /// Per chain: the version of its progress last seen, and the piece being written then.
    seen: Vec<(u64, u64)>,
}

impl Follower {
    /// Wait up to `timeout` on each chain in turn for a change: the pieces begun since the last call.
    ///
    /// # Errors
    ///
    /// The memory pieces of a chain failed.
    pub fn next(&mut self, chains: &[MemChain], timeout: Duration) -> Result<Vec<Cut>, MemError> {
        self.seen.resize(chains.len(), (0, 0));
        let mut cuts = Vec::new();
        for (chain, (c, (version, piece))) in chains.iter().zip(&mut self.seen).enumerate() {
            let Some(p) = c.watch.wait_past_for(*version, timeout) else {
                continue;
            };
            *version = p.version;
            if p.piece > *piece {
                *piece = p.piece;
                cuts.push(Cut {
                    chain,
                    piece: p.piece,
                    held: c.pieces.keys()?.len(),
                });
            }
        }
        Ok(cuts)
    }
}

/// A capture into the memory window, running: its capture thread and a triage thread per chain.
///
/// [Self::chains] hands out the window while it runs, so a viewer follows it live; [Self::join] waits for the end.
pub struct Running<X, C, K: PieceStore, A: Analyser> {
    chains: Vec<MemChain>,
    capture: JoinHandle<Captured<X, C>>,
    #[expect(
        clippy::type_complexity,
        reason = "a triage thread's result and the chain it hands back"
    )]
    triages: Vec<
        JoinHandle<(
            Result<Triaged, WindowError<K::Error, A::Error>>,
            Chain<K, A>,
        )>,
    >,
}

/// What the capture thread hands back: what `record` returned, each mux's close and the time until the last one.
struct Captured<X, C> {
    recorded: Result<X, C>,
    closed: Vec<Result<(), MuxError<MemError>>>,
    capture_time: Duration,
}

impl<X, C, K: PieceStore, A: Analyser> Running<X, C, K, A> {
    /// The window's chains in the order of the chains given to [start], each the memory pieces of its mux.
    pub fn chains(&self) -> &[MemChain] {
        &self.chains
    }

    /// Whether the capture and every triage ended.
    pub fn is_finished(&self) -> bool {
        self.capture.is_finished() && self.triages.iter().all(JoinHandle::is_finished)
    }

    /// Wait until the capture and every triage ended: what they did, and each chain's kept store and analyser.
    ///
    /// # Errors
    ///
    /// The capture's error, a mux's, a triage's, or a thread that panicked.
    #[expect(
        clippy::type_complexity,
        reason = "what ran, and the chains handed back"
    )]
    pub fn join(self) -> Result<(Ran<X>, Vec<Chain<K, A>>), RunError<C, K::Error, A::Error>> {
        // a mux closed, or dropped after a failed capture, ends its triage
        let captured = self
            .capture
            .join()
            .map_err(|_panic| RunError::Thread("the capture"))?;
        let mut triaged = Vec::with_capacity(self.triages.len());
        let mut chains = Vec::with_capacity(self.triages.len());
        for t in self.triages {
            let (t, chain) = t.join().map_err(|_panic| RunError::Thread("a triage"))?;
            triaged.push(t);
            chains.push(chain);
        }
        let recorded = captured.recorded.map_err(RunError::Capture)?;
        for c in captured.closed {
            c.map_err(RunError::Mux)?;
        }
        let ran = Ran {
            recorded,
            capture_time: captured.capture_time,
            triaged: triaged
                .into_iter()
                .collect::<Result<_, _>>()
                .map_err(RunError::Window)?,
        };
        Ok((ran, chains))
    }
}

/// Start a capture into the memory window, a chain in memory for each of `chains`, each read by a triage of its own.
///
/// `record` writes the capture through a mux per chain, each into a chain in memory cut by `options`, each piece
/// held apart, on a capture thread. [triage] runs each chain's analyser over it on a thread of its own, `window` holding
/// each chain's pieces.
///
/// The mux of `chains[c]` writes chain `c`. A mux has one writing thread, so a capture faster than one thread copies
/// spreads over several chains. The muxes close once `record` returns, so `record` ends its handles first.
///
/// # Errors
///
/// No chain, a mux that didn't start, or a thread that didn't.
#[expect(
    clippy::type_complexity,
    reason = "the running capture, generic over what it runs"
)]
pub fn start<K, A, X, C>(
    owner: OwnerId,
    options: ChainOptions,
    window: Window,
    chains: Vec<Chain<K, A>>,
    record: impl FnOnce(&[&TapeMux<MemPieces>]) -> Result<X, C> + Send + 'static,
) -> Result<Running<X, C, K, A>, RunError<C, K::Error, A::Error>>
where
    K: PieceStore + Send + 'static,
    A: Analyser + Send + 'static,
    A::Error: Send,
    K::Error: Send,
    X: Send + 'static,
    C: Send + 'static,
{
    if chains.is_empty() {
        return Err(RunError::NoChain);
    }
    let options = ChainOptions {
        tapes: PieceTapes::Each,
        ..options
    };
    let rams: Vec<MemPieces> = chains.iter().map(|_| MemPieces::new()).collect();
    let muxes = rams
        .iter()
        .zip(0..)
        .map(|(ram, chain)| {
            TapeMux::create(
                ram.clone(),
                owner,
                chain,
                &[],
                options.clone(),
                MuxOptions::default(),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(RunError::Mux)?;
    let mem: Vec<MemChain> = rams
        .into_iter()
        .zip(&muxes)
        .map(|(pieces, mux)| MemChain {
            pieces,
            watch: mux.watch(),
        })
        .collect();
    let mut triages = Vec::with_capacity(chains.len());
    for (mut c, m) in chains.into_iter().zip(mem.iter().cloned()) {
        let t = std::thread::Builder::new()
            .name("triage".to_owned())
            .spawn(move || {
                let t = triage(&m.pieces, &m.watch, window, &mut c.kept, &mut c.analyser);
                (t, c)
            })
            .map_err(RunError::Spawn)?;
        triages.push(t);
    }
    let start = Instant::now();
    let capture = std::thread::Builder::new()
        .name("capture".to_owned())
        .spawn(move || {
            let recorded = record(&muxes.iter().collect::<Vec<_>>());
            let closed = muxes
                .into_iter()
                .map(|m| m.close().map(|_pieces| ()))
                .collect();
            Captured {
                recorded,
                closed,
                capture_time: start.elapsed(),
            }
        })
        .map_err(RunError::Spawn)?;
    Ok(Running {
        chains: mem,
        capture,
        triages,
    })
}

/// Capture into the memory window and wait for the end: [start], then [Running::join].
///
/// # Errors
///
/// What [start] and [Running::join] fail with.
#[expect(
    clippy::type_complexity,
    reason = "what ran, and the chains handed back"
)]
pub fn run<K, A, X, C>(
    owner: OwnerId,
    options: ChainOptions,
    window: Window,
    chains: Vec<Chain<K, A>>,
    record: impl FnOnce(&[&TapeMux<MemPieces>]) -> Result<X, C> + Send + 'static,
) -> Result<(Ran<X>, Vec<Chain<K, A>>), RunError<C, K::Error, A::Error>>
where
    K: PieceStore + Send + 'static,
    A: Analyser + Send + 'static,
    A::Error: Send,
    K::Error: Send,
    X: Send + 'static,
    C: Send + 'static,
{
    start(owner, options, window, chains, record)?.join()
}

/// What a capture into the memory window gives: what the capture returned and what each chain's triage did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran<X> {
    pub recorded: X,
    /// From the muxes' start until they closed: every byte of the capture in memory, the triage perhaps still
    /// reading.
    pub capture_time: Duration,
    /// A [Triaged] per chain, in the order of the chains.
    pub triaged: Vec<Triaged>,
}

/// Why a capture into the memory window failed: `C` is the capture's error, `K` the kept store's, `A` the analyser's.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunError<C, K, A> {
    /// [start] was given no chain.
    NoChain,
    Capture(C),
    /// A mux of a chain in memory.
    Mux(MuxError<MemError>),
    Window(WindowError<K, A>),
    /// A thread didn't start.
    Spawn(std::io::Error),
    /// A thread panicked.
    Thread(&'static str),
}

impl<C: core::fmt::Debug, K: core::fmt::Debug, A: core::fmt::Debug> core::fmt::Display
    for RunError<C, K, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoChain => f.write_str("a capture into memory needs a chain"),
            Self::Capture(e) => write!(f, "the capture: {e:?}"),
            Self::Mux(e) => write!(f, "a mux in memory: {e:?}"),
            Self::Window(e) => write!(f, "{e}"),
            Self::Spawn(e) => write!(f, "a thread of the capture didn't start: {e}"),
            Self::Thread(what) => write!(f, "{what} panicked"),
        }
    }
}

impl<C: core::fmt::Debug, K: core::fmt::Debug, A: core::fmt::Debug> core::error::Error
    for RunError<C, K, A>
{
}

/// Why an analyser failed: `P` is what reading a piece in memory failed with, `R` the error of the store its results
/// go to.
#[derive(Debug)]
#[non_exhaustive]
pub enum LookError<P, R> {
    /// A piece in memory that doesn't read as the capture writes it.
    Piece(P),
    /// The mux of the results, or a handle of it.
    Results(MuxError<R>),
}

impl<P, R> From<MuxError<R>> for LookError<P, R> {
    fn from(e: MuxError<R>) -> Self {
        Self::Results(e)
    }
}

/// A track `kept` of an analyser's results under `key`, on `clock`: a span per piece of its chain kept, from its
/// first item's tick to its last one's, the piece's sequence number as 8 bytes little-endian.
pub fn kept_spec(key: TrackKey, clock: ClockNum) -> TrackSpec<'static> {
    TrackSpec {
        key,
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: "kept",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef {
            ends: Ends::Spans,
            flags: false,
        }),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// The pieces a window kept, for an analyser's track `kept` ([kept_spec]): noted as each is kept, pushed in time
/// order at the end.
///
/// A piece holds chunks of several tracks, each cut where it filled, so a later piece may start before an earlier
/// one. The log sorts the spans by their start before it pushes them.
pub struct KeptLog<T>
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    handle: EventsHandle<T>,
    spans: Vec<(i64, i64, u64)>,
}

impl<T> KeptLog<T>
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    /// A log pushing into `handle`, the handle of a track [kept_spec] defines.
    pub fn new(handle: EventsHandle<T>) -> Self {
        Self {
            handle,
            spans: Vec::new(),
        }
    }

    /// Note piece `seq`, kept: its span ([span]). A piece without chunks takes no item.
    ///
    /// # Errors
    ///
    /// A piece whose index doesn't read.
    pub fn note(
        &mut self,
        seq: u64,
        piece: &TapeReader<MemPiece>,
    ) -> Result<(), ReadError<MemError>> {
        if let Some((start, end)) = span(piece)? {
            self.spans.push((start, end, seq));
        }
        Ok(())
    }

    /// Push every piece noted, in the order of their starts, and end the handle.
    ///
    /// # Errors
    ///
    /// A push the mux refused.
    pub fn finish(mut self) -> Result<(), MuxError<T::Error>> {
        self.spans.sort_unstable();
        for (start, end, seq) in &self.spans {
            self.handle.push_all(&[Event {
                start: *start,
                end: *end,
                payload: &seq.to_le_bytes(),
                ..Event::default()
            }])?;
        }
        self.handle.finish()
    }
}

/// The ticks of the first and the last item of a piece: the smallest first tick and the largest last tick of its
/// chunks' index entries; None for a piece without chunks.
///
/// # Errors
///
/// A piece whose index doesn't read.
pub fn span(piece: &TapeReader<MemPiece>) -> Result<Option<(i64, i64)>, ReadError<MemError>> {
    let mut span: Option<(i64, i64)> = None;
    for e in piece.entries(0)? {
        if !matches!(
            e.kind,
            ww_tape::BlockKind::Events | ww_tape::BlockKind::Series
        ) {
            continue;
        }
        span = Some(span.map_or((e.first_tick, e.last_tick), |(a, b)| {
            (a.min(e.first_tick), b.max(e.last_tick))
        }));
    }
    Ok(span)
}

/// Bytes the pieces of `ram` hold, the one being written included.
fn held<K, A>(ram: &MemPieces) -> Result<u64, WindowError<K, A>> {
    let mut held = 0;
    for key in ram.keys().map_err(WindowError::Ram)? {
        if let Some(tape) = ram.get(&key).map_err(WindowError::Ram)? {
            held += tape.held().map_err(WindowError::Ram)?;
        }
    }
    Ok(held)
}

/// Why [triage] stopped: `K` is the kept store's error, `A` the analyser's.
#[derive(Debug)]
#[non_exhaustive]
pub enum WindowError<K, A> {
    /// The memory pieces failed.
    Ram(MemError),
    /// A piece in memory didn't open.
    Read(ReadError<MemError>),
    /// A piece read and held for a later verdict left memory before it.
    Gone(u64),
    /// The kept store refused a piece or failed.
    Kept(K),
    Analyser(A),
}

impl<K: core::fmt::Debug, A: core::fmt::Debug> core::fmt::Display for WindowError<K, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Ram(e) => write!(f, "the memory pieces: {e}"),
            Self::Read(e) => write!(f, "a piece in memory: {e:?}"),
            Self::Gone(seq) => write!(f, "piece {seq} left memory before its verdict"),
            Self::Kept(e) => write!(f, "the kept store: {e:?}"),
            Self::Analyser(e) => write!(f, "the analyser: {e:?}"),
        }
    }
}

impl<K: core::fmt::Debug, A: core::fmt::Debug> core::error::Error for WindowError<K, A> {}
