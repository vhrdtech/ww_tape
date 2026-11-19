//! Writers of files named by a path.

use std::path::Path;

use ww_tape::sink::sink_err;
use ww_tape::writer::{Closed, Resumed, SyncPolicy, TapeWriter, WriteError, WriterOptions};

use crate::sink::{FileSink, Files, FsError, none_at};

/// A new file at `path`, written as `path.partial` until [TapeWriter::close]; fails if `path` is there or a
/// `path.partial` holds bytes ([FileSink::create]).
pub fn create(
    path: &Path,
    policy: SyncPolicy,
) -> Result<TapeWriter<FileSink>, WriteError<FsError>> {
    let sink = FileSink::create(path).map_err(sink_err("creating a writer"))?;
    Ok(TapeWriter::create_on(sink, policy))
}

/// Append to the file at `path` in place, created if it isn't there, see [TapeWriter::append_on].
pub fn append(
    path: &Path,
    policy: SyncPolicy,
) -> Result<TapeWriter<FileSink>, WriteError<FsError>> {
    let sink = FileSink::append(path).map_err(sink_err("opening a writer to append"))?;
    TapeWriter::append_on(sink, policy)
}

/// Reopen the unclosed piece `path` (written as `path.partial` by [create] and cut off by a crash) to go on
/// writing it, see [TapeWriter::resume_on]. With neither `path` nor `path.partial` there, its directory missing too,
/// the error is [FsError::NoPiece] naming `path`.
pub fn resume(
    path: &Path,
    options: WriterOptions,
) -> Result<(TapeWriter<FileSink>, Resumed), WriteError<FsError>> {
    let what = "opening a writer to resume";
    let refused = |error| WriteError::Sink { what, error };
    // checked before the lock, which a missing directory fails as an I/O error
    if none_at(path) {
        return Err(refused(FsError::NoPiece(path.to_path_buf())));
    }
    match FileSink::reopen(path).map_err(sink_err(what))? {
        Some(sink) => TapeWriter::resume_on(sink, options),
        None if path.exists() => Err(refused(FsError::Exists(path.to_path_buf()))),
        None => Err(refused(FsError::NoPiece(path.to_path_buf()))),
    }
}

/// End the piece `writer` writes by a cut, its close record naming the next piece, and start the next piece
/// of the chain in a new file at `next_path`, see [TapeWriter::cut_with].
pub fn cut(
    writer: TapeWriter<FileSink>,
    next_path: &Path,
) -> Result<(Closed, TapeWriter<FileSink>), WriteError<FsError>> {
    writer.cut_with(&mut Files::file(next_path), &0)
}
