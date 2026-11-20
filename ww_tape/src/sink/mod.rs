//! [TapeSink]: where a [TapeWriter](crate::TapeWriter) puts its bytes, memory ([MemSink]) or a file (`FileSink` of
//! the crate `ww_tape_fs`).
//!
//! A sink appends, syncs, closes and, for [TapeWriter::resume_on](crate::TapeWriter::resume_on), reads back and
//! truncates. A store of [Tapes] makes the sink of each piece of a [PieceChain](crate::PieceChain) by its key:
//! [MemTapes] keeps each piece in memory under its key, where a [TapeReader](crate::TapeReader) reads it while it
//! is written ([MemTape] is a [ByteSource]); `ww_tape_fs::Files` keeps each piece in a file of a directory, created
//! as `NAME.partial` and renamed to `NAME` at the close.
//!
//! A chain may also keep all its pieces in one file or memory tape ([Tapes::create_whole],
//! [Tapes::open_whole]): the writer of the next piece goes on in the same sink, whose window
//! ([TapeSink::base], [TapeSink::rebase]) starts where the piece before ended, so offsets, reads and truncates count
//! from the piece's start as in a file of its own.
//!
//! A sink is the only writer of its bytes while it lives: a memory sink marks its tape held, a file sink holds an
//! exclusive lock on the lock file `NAME.lock` beside it. A second sink of the same bytes is refused
//! ([MemError::Locked], `ww_tape_fs::FsError::Locked`), so two writers never fork one tape.

use crate::reader::ByteSource;
use crate::writer::WriteError;

#[cfg(feature = "std")]
mod mem;
#[cfg(feature = "std")]
pub use mem::{MemError, MemSink, MemTape, MemTapes};

/// Where a [TapeWriter](crate::TapeWriter) appends its bytes.
pub trait TapeSink {
    /// Why an operation failed, or the sink's store refused ([Tapes::Error]).
    type Error: core::error::Error + 'static;

    /// Append `bytes` at the end.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Make the bytes written so far durable.
    fn sync(&mut self) -> Result<(), Self::Error>;

    /// The bytes are whole: a file takes its final name and its directory is synced; memory does nothing. Called
    /// once, after the last sync.
    fn close(&mut self) -> Result<(), Self::Error>;

    /// Bytes in the sink.
    fn size(&self) -> Result<u64, Self::Error>;

    /// Fill `buf` from `offset`, within [Self::size].
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Drop every byte from `len` on: what resume keeps.
    fn truncate(&mut self, len: u64) -> Result<(), Self::Error>;

    /// Make room for `bytes` more, so that appending them never moves what is written: a hint, nothing changes when
    /// it can't be met. A file does nothing.
    fn reserve(&mut self, _bytes: u64) {}

    /// Where the piece being written starts in the bytes: [Self::size], [Self::read_at] and [Self::truncate] count
    /// from here. 0 unless [Self::rebase] moved it.
    fn base(&self) -> u64 {
        0
    }

    /// Whether the sink holds pieces back to back ([Tapes::create_whole], [Tapes::open_whole], a file
    /// appended to in place), so a writer may cut in place ([TapeWriter::cut_in_place](crate::TapeWriter::cut_in_place)).
    /// A sink of one piece (`NAME.partial`, a piece of a chain of files) doesn't.
    fn holds_pieces(&self) -> bool {
        false
    }

    /// Let the piece being written start at `base` in the bytes, at most their whole length: the next piece of a
    /// file of several, or the last piece of one to resume. A sink that holds one piece only refuses any base but 0.
    fn rebase(&mut self, base: u64) -> Result<(), Self::Error>;
}

/// Why [check_base] refuses a new base.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BaseRefused {
    /// A sink of one piece takes no base but 0.
    OnePiece,
    /// The base is past the end of the bytes.
    PastEnd,
}

impl core::fmt::Display for BaseRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::OnePiece => "a sink of one piece takes no base but 0",
            Self::PastEnd => "the base is past the end of the bytes",
        })
    }
}

impl core::error::Error for BaseRefused {}

/// A new `base` of a sink whose bytes are `whole` long, checked for [TapeSink::rebase]: past the end refused, and any
/// base but 0 for a sink of one piece (`pieces` false).
pub fn check_base(base: u64, whole: u64, pieces: bool) -> Result<u64, BaseRefused> {
    if base != 0 && !pieces {
        return Err(BaseRefused::OnePiece);
    }
    if base > whole {
        return Err(BaseRefused::PastEnd);
    }
    Ok(base)
}

/// A store of tapes by key: makes the tape of each piece of a [PieceChain](crate::PieceChain), reopens one to
/// write on and reads a closed one. A store keeps one writer per tape: a second sink of a tape a sink holds is
/// refused.
pub trait Tapes {
    /// What names a tape in the store.
    type Key: Ord + Clone + core::fmt::Debug;
    /// Why an operation failed or was refused: the tape is there already, another writer holds it.
    type Error: core::error::Error + 'static;
    type Sink: TapeSink<Error = Self::Error>;
    /// A closed tape, read back.
    type Source: ByteSource<Error = Self::Error>;

    /// The key of the tape of piece `seq` of a chain of a tape per piece ([PieceTapes::Each](crate::PieceTapes)),
    /// or, for `seq` 0, of the tape that holds every piece ([PieceTapes::One](crate::PieceTapes)).
    fn key_of(&self, seq: u64) -> Self::Key;

    /// A new sink for `key`; refused when the tape is there, closed or not.
    fn create(&mut self, key: &Self::Key) -> Result<Self::Sink, Self::Error>;

    /// The sink of `key` to write on: its unclosed tape, to resume, or its closed one, to write on after its last
    /// close record ([TapeWriter::resume_on](crate::TapeWriter::resume_on)); None when there is none.
    fn reopen(&mut self, key: &Self::Key) -> Result<Option<Self::Sink>, Self::Error>;

    /// The closed bytes of `key`; None when the tape isn't closed.
    fn closed(&self, key: &Self::Key) -> Result<Option<Self::Source>, Self::Error>;

    /// A new sink for a tape of several pieces under `key`, under its final name from the start (a crash leaves its
    /// last piece unclosed, which a resume finds); refused when the tape is there.
    fn create_whole(&mut self, key: &Self::Key) -> Result<Self::Sink, Self::Error>;

    /// The tape of several pieces under `key`, appended to in place, closed or not; None when there is none.
    fn open_whole(&mut self, key: &Self::Key) -> Result<Option<Self::Sink>, Self::Error>;
}

/// A sink's error as the [WriteError::Sink] of `what` the writer was doing, for `map_err` where a sink is opened.
pub fn sink_err<E>(what: &'static str) -> impl FnOnce(E) -> WriteError<E> {
    move |error| WriteError::Sink { what, error }
}
