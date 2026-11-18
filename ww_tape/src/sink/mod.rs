//! [TapeSink]: where a [TapeWriter](crate::TapeWriter) puts its bytes, a file ([FileSink]) or memory ([MemSink]).
//!
//! A sink appends, syncs, finishes and, for [TapeWriter::resume_on](crate::TapeWriter::resume_on), reads back and
//! truncates. A [SinkFactory] makes the sink of each piece of a [PieceChain](crate::PieceChain) by its path: [Files]
//! creates `NAME.partial` and renames it to `NAME` at the finish, [MemTapes] keeps each piece in memory under its
//! path, where a [TapeReader](crate::TapeReader) reads it while it is written ([MemTape] is a [ByteSource]).
//!
//! A chain may also keep all its pieces in one file or memory tape ([SinkFactory::create_whole],
//! [SinkFactory::open_whole]): the writer of the next piece goes on in the same sink, whose window
//! ([TapeSink::base], [TapeSink::rebase]) starts where the piece before ended, so offsets, reads and truncates count
//! from the piece's start as in a file of its own.
//!
//! A sink is the only writer of its bytes while it lives: a file sink holds an exclusive lock on its file
//! ([File::try_lock](std::fs::File::try_lock), advisory: other writers of the format take it too), a memory sink marks its tape held. A second
//! sink of the same bytes is refused with [WriteError::Locked], so two writers never fork one file.

use std::io;
use std::path::Path;

use crate::reader::ByteSource;
use crate::writer::WriteError;

mod fs;
mod mem;
pub use fs::{FileSink, FileSource, Files, partial_name};
pub use mem::{MemSink, MemTape, MemTapes};

/// Where a [TapeWriter](crate::TapeWriter) appends its bytes.
pub trait TapeSink {
    /// Append `bytes` at the end.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Make the bytes written so far durable.
    fn sync(&mut self) -> io::Result<()>;

    /// The bytes are whole: a file takes its final name and its directory is synced; memory does nothing. Called
    /// once, after the last sync.
    fn finish(&mut self) -> io::Result<()>;

    /// Bytes in the sink.
    fn size(&self) -> io::Result<u64>;

    /// Fill `buf` from `offset`, within [Self::size].
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Drop every byte from `len` on: what resume keeps.
    fn truncate(&mut self, len: u64) -> io::Result<()>;

    /// Make room for `bytes` more, so that appending them never moves what is written: a hint, nothing changes when
    /// it can't be met. A file does nothing.
    fn reserve(&mut self, _bytes: u64) {}

    /// The name of the bytes once finished.
    fn path(&self) -> &Path;

    /// The name of the bytes while they are written (`NAME.partial` for a new file).
    fn current_path(&self) -> &Path {
        self.path()
    }

    /// Where the piece being written starts in the bytes: [Self::size], [Self::read_at] and [Self::truncate] count
    /// from here. 0 unless [Self::rebase] moved it.
    fn base(&self) -> u64 {
        0
    }

    /// Whether the sink holds pieces back to back ([SinkFactory::create_whole], [SinkFactory::open_whole], a file
    /// appended to in place), so a writer may cut in place ([TapeWriter::cut_in_place](crate::TapeWriter::cut_in_place)).
    /// A sink of one piece (`NAME.partial`, a piece of a chain of files) doesn't.
    fn holds_pieces(&self) -> bool {
        false
    }

    /// Let the piece being written start at `base` in the bytes, at most their whole length: the next piece of a
    /// file of several, or the last piece of one to resume. A sink that holds one piece only refuses any base but 0
    /// ([io::ErrorKind::Unsupported]).
    fn rebase(&mut self, base: u64) -> io::Result<()> {
        if base == 0 {
            Ok(())
        } else {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

/// The whole length of the bytes of a sink whose window starts at `base` and holds `size` bytes, checked against a
/// new base; any base but 0 refused for a sink of one piece (`pieces` false).
fn check_base(base: u64, whole: u64, pieces: bool) -> io::Result<u64> {
    if base != 0 && !pieces {
        return Err(io::ErrorKind::Unsupported.into());
    }
    if base > whole {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a piece can't start past the end of the bytes",
        ));
    }
    Ok(base)
}

/// Makes the sink of each piece of a chain by its path, reopens an unfinished one and reads a finished one.
pub trait SinkFactory {
    type Sink: TapeSink;
    /// A finished piece, read back.
    type Source: ByteSource<Error = io::Error>;

    /// A new sink for `path`; [WriteError::Exists] when `path` is there, finished or not.
    fn create(&mut self, path: &Path) -> Result<Self::Sink, WriteError>;

    /// The unfinished sink of `path`, to resume; None when there is none.
    fn reopen(&mut self, path: &Path) -> Result<Option<Self::Sink>, WriteError>;

    /// The finished bytes of `path`; None when `path` isn't finished.
    fn finished(&self, path: &Path) -> Result<Option<Self::Source>, WriteError>;

    /// A new sink for a file of several pieces at `path`, written under its own name from the start (a crash
    /// leaves its last piece unfinished, which a resume finds); [WriteError::Exists] when `path` is there. A
    /// factory without files of several pieces refuses ([io::ErrorKind::Unsupported]).
    fn create_whole(&mut self, path: &Path) -> Result<Self::Sink, WriteError> {
        Err(unsupported(path))
    }

    /// The file of several pieces at `path`, appended to in place, finished or not; None when there is none.
    fn open_whole(&mut self, path: &Path) -> Result<Option<Self::Sink>, WriteError> {
        Err(unsupported(path))
    }
}

/// A factory that holds no files of several pieces.
fn unsupported(_path: &Path) -> WriteError {
    WriteError::Io {
        what: "opening a file of several pieces",
        error: io::ErrorKind::Unsupported.into(),
    }
}

/// `offset` in a piece starting at `base`, from the start of the bytes.
fn past_base(base: u64, offset: u64) -> io::Result<u64> {
    base.checked_add(offset)
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
}

pub(crate) fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> WriteError {
    move |error| WriteError::Io { what, error }
}
