//! A capture into memory at line rate: a window of the newest pieces in RAM, an analyser that reads each piece as it
//! closes, and only the pieces worth keeping copied to a store of their own.
//!
//! A [TapeMux] writes the capture into a chain of pieces in a [MemTapes], a tape per piece
//! ([PieceTapes::Each](ww_tape::PieceTapes)), cut by size. [triage] runs beside it in the same process. It waits on
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

use ww_tape::reader::ReadError;
use ww_tape::{ByteSource, MemError, MemTape, MemTapes, TapeReader, TapeSink, Tapes};
use ww_tape_mux::ProgressWatch;

/// Bytes [triage] copies from memory to the kept store in one write.
pub const COPY_BYTES: usize = 4 << 20;

/// What a window of a capture in memory keeps, and how much memory it may take.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Bytes the pieces in memory take at most, the piece being written included. Past it the oldest pieces leave
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
                    Some(&first) if first <= seq && ram.closed(&first).map_err(WindowError::Ram)?.is_some() => {
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
            let n = usize::try_from(end - at).unwrap_or(COPY_BYTES).min(COPY_BYTES);
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
