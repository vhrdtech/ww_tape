//! A capture into memory at line rate: a window of the newest pieces in RAM, an analyser that reads each piece as it
//! closes, and only the pieces worth keeping copied to a store of their own.
//!
//! A [TapeMux] writes the capture into a chain of pieces in a [MemTapes], a tape per piece
//! ([PieceTapes::Each](ww_tape::PieceTapes)), cut by size. A mux has one writing thread, so [run] spreads a capture
//! faster than one thread copies over several chains, each with a mux of its own. [triage] runs beside each chain in
//! the same process, on a thread of its own. It waits on
//! the mux's [ProgressWatch], opens each piece once it closes ([TapeReader] over the [MemTape], no copy) and asks the
//! [Analyser] whether to keep it. A kept piece is copied whole to the kept store, such as a directory of piece files,
//! where it reads alone. Every other piece leaves memory ([MemTapes::remove]).
//!
//! [Window::before] and [Window::after] keep the pieces around a kept one too, so the analyser's verdict on one piece
//! keeps what led up to it and what followed. The pieces before wait in memory, already read, until no later verdict
//! can keep them. When the analyser falls behind and the window holds more than [Window::bytes_max], the oldest
//! pieces go unread: [Triaged::overrun] counts them, as a capture card counts the frames it drops.
//!
//! ```
//! use ww_tape::{ChainOptions, CutPolicy, MemTapes, OwnerId, PieceTapes};
//! use ww_tape_example::window::{Analyser, Window, triage};
//! use ww_tape_mux::{MuxOptions, TapeMux};
//!
//! // keeps every third piece
//! struct Third;
//! impl Analyser for Third {
//!     type Error = std::convert::Infallible;
//!     fn look(&mut self, seq: u64, _: &ww_tape::TapeReader<ww_tape::MemTape>) -> Result<bool, Self::Error> {
//!         Ok(seq % 3 == 0)
//!     }
//! }
//!
//! let ram = MemTapes::new();
//! let options = ChainOptions {
//!     cut: CutPolicy { bytes: exact_units::Bytes::new(64 << 10), ..CutPolicy::default() },
//!     tapes: PieceTapes::Each,
//!     ..ww_tape_example::ethernet::chain_options()
//! };
//! let mux = TapeMux::create(ram.clone(), OwnerId([0xe7; 32]), 0, &[], options, MuxOptions::default())?;
//! let watch = mux.watch();
//! let link = ww_tape_example::ethernet::Link::full().micros(500);
//! let mut kept = MemTapes::new();
//! let triaged = std::thread::scope(|s| {
//!     let t = s.spawn(|| triage(&ram, &watch, Window::default(), &mut kept, &mut Third));
//!     link.record(&[&mux]).expect("the capture");
//!     mux.close().expect("the close");
//!     t.join().expect("the triage")
//! })?;
//! assert_eq!(triaged.kept, (0..triaged.pieces).filter(|s| s % 3 == 0).collect::<Vec<_>>());
//! assert!(ram.keys()?.is_empty(), "every piece left memory");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use shrink_wrap::prelude::*;
use ww_tape::reader::ReadError;
use ww_tape::{
    ByteSource, ChainOptions, ClockNum, Ends, EventsDef, Items, MemError, MemTape, MemTapes,
    OwnerId, PieceTapes, TapeReader, TapeSink, Tapes, TrackKey, TrackKind, TrackRepeat, TrackSpec,
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
    fn look(&mut self, seq: u64, piece: &TapeReader<MemTape>) -> Result<bool, Self::Error>;

    /// Piece `seq` reached the kept store; `piece` reads it. Called in the order of `seq`.
    fn kept(&mut self, _seq: u64, _piece: &TapeReader<MemTape>) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<A: Analyser + ?Sized> Analyser for &mut A {
    type Error = A::Error;

    fn look(&mut self, seq: u64, piece: &TapeReader<MemTape>) -> Result<bool, A::Error> {
        (**self).look(seq, piece)
    }

    fn kept(&mut self, seq: u64, piece: &TapeReader<MemTape>) -> Result<(), A::Error> {
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

/// Run the window over the capture that `watch`'s mux writes into `ram`, a tape per piece, until the mux ends.
///
/// `analyser` reads every piece as it closes. The kept ones go to `kept` under their own sequence numbers, and every
/// piece leaves memory once no verdict can keep it.
///
/// # Errors
///
/// A piece that doesn't read or copy, a kept store that refused a piece, or the analyser's error.
pub fn triage<K: Tapes, A: Analyser>(
    ram: &MemTapes,
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
    fn make_room<K, A>(&mut self, ram: &MemTapes, seq: u64) -> Result<bool, WindowError<K, A>> {
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
    fn piece<K: Tapes, A: Analyser>(
        &mut self,
        ram: &MemTapes,
        seq: u64,
        tape: &MemTape,
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
    fn keep<K: Tapes, A: Analyser>(
        &mut self,
        ram: &MemTapes,
        seq: u64,
        tape: &MemTape,
        reader: &TapeReader<MemTape>,
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

/// A chain of a capture in memory as [run] reads it: the store its kept pieces go to and its analyser.
pub struct Chain<'k, K, A> {
    pub kept: &'k mut K,
    pub analyser: A,
}

/// Capture into a window in memory, a chain in memory for each of `chains`, each read by a triage of its own.
///
/// `record` writes the capture through a mux per chain, each into a chain in memory cut by `options`, a tape per
/// piece. [triage] runs each chain's analyser over it on a thread of its own, `window` holding each chain's pieces.
///
/// The mux of `chains[c]` writes chain `c`. A mux has one writing thread, so a capture faster than one thread copies
/// spreads over several chains. What `record` returned and what each chain's triage did. The muxes close once
/// `record` returns, so `record` ends its handles first.
///
/// # Errors
///
/// No chain, the capture's error, a mux's, or a triage's.
pub fn run<K, A, X, C>(
    owner: OwnerId,
    options: ChainOptions,
    window: Window,
    chains: &mut [Chain<'_, K, A>],
    record: impl FnOnce(&[&TapeMux<MemTapes>]) -> Result<X, C>,
) -> Result<Ran<X>, RunError<C, K::Error, A::Error>>
where
    K: Tapes + Send,
    A: Analyser + Send,
    A::Error: Send,
    K::Error: Send,
{
    if chains.is_empty() {
        return Err(RunError::NoChain);
    }
    let options = ChainOptions {
        tapes: PieceTapes::Each,
        ..options
    };
    let rams: Vec<MemTapes> = chains.iter().map(|_| MemTapes::new()).collect();
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
    let watches: Vec<ProgressWatch> = muxes.iter().map(TapeMux::watch).collect();
    let start = Instant::now();
    std::thread::scope(|s| {
        let triages: Vec<_> = chains
            .iter_mut()
            .zip(&rams)
            .zip(&watches)
            .map(|((c, ram), watch)| {
                s.spawn(move || triage(ram, watch, window, c.kept, &mut c.analyser))
            })
            .collect();
        let recorded = record(&muxes.iter().collect::<Vec<_>>());
        // a mux closed, or dropped after a failed capture, ends its triage
        let closed: Vec<_> = muxes.into_iter().map(TapeMux::close).collect();
        let capture_time = start.elapsed();
        let mut triaged = Vec::with_capacity(triages.len());
        for t in triages {
            let t = t.join().map_err(|_panic| RunError::Thread("a triage"))?;
            triaged.push(t);
        }
        let recorded = recorded.map_err(RunError::Capture)?;
        for c in closed {
            c.map_err(RunError::Mux)?;
        }
        Ok(Ran {
            recorded,
            capture_time,
            triaged: triaged
                .into_iter()
                .collect::<Result<_, _>>()
                .map_err(RunError::Window)?,
        })
    })
}

/// What [run] gives: what the capture returned and what each chain's triage did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran<X> {
    pub recorded: X,
    /// From the muxes' start until they closed: every byte of the capture in memory, the triage perhaps still
    /// reading.
    pub capture_time: Duration,
    /// A [Triaged] per chain, in the order of the chains.
    pub triaged: Vec<Triaged>,
}

/// Why [run] failed: `C` is the capture's error, `K` the kept store's, `A` the analyser's.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunError<C, K, A> {
    /// [run] was given no chain.
    NoChain,
    Capture(C),
    /// A mux of a chain in memory.
    Mux(MuxError<MemError>),
    Window(WindowError<K, A>),
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
    T: Tapes + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    handle: EventsHandle<T>,
    spans: Vec<(i64, i64, u64)>,
}

impl<T> KeptLog<T>
where
    T: Tapes + Send + 'static,
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
        piece: &TapeReader<MemTape>,
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
pub fn span(piece: &TapeReader<MemTape>) -> Result<Option<(i64, i64)>, ReadError<MemError>> {
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

/// Bytes the tapes of `ram` hold, the one being written included.
fn held<K, A>(ram: &MemTapes) -> Result<u64, WindowError<K, A>> {
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
    /// The memory tapes failed.
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
            Self::Ram(e) => write!(f, "the memory tapes: {e}"),
            Self::Read(e) => write!(f, "a piece in memory: {e:?}"),
            Self::Gone(seq) => write!(f, "piece {seq} left memory before its verdict"),
            Self::Kept(e) => write!(f, "the kept store: {e:?}"),
            Self::Analyser(e) => write!(f, "the analyser: {e:?}"),
        }
    }
}

impl<K: core::fmt::Debug, A: core::fmt::Debug> core::error::Error for WindowError<K, A> {}
