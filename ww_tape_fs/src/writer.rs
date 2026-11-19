//! Writers of files named by a path.

use std::path::Path;

use ww_tape::sink::sink_err;
use ww_tape::writer::{
    Finished, ResumeError, Resumed, SyncPolicy, TapeWriter, WriteError, WriterOptions,
};

use crate::sink::{FileSink, Files, FsError};

/// A new file at `path`, written as `path.partial` until [TapeWriter::finish]; fails if `path` is there or a
/// `path.partial` holds bytes ([FileSink::create]).
pub fn create(
    path: &Path,
    policy: SyncPolicy,
) -> Result<TapeWriter<FileSink>, WriteError<FsError>> {
    let sink = FileSink::create(path).map_err(sink_err("creating the file"))?;
    Ok(TapeWriter::create_on(sink, policy))
}

/// Append to the file at `path` in place, created if it isn't there, see [TapeWriter::append_on].
pub fn append(
    path: &Path,
    policy: SyncPolicy,
) -> Result<TapeWriter<FileSink>, WriteError<FsError>> {
    let sink = FileSink::append(path).map_err(sink_err("opening the file"))?;
    TapeWriter::append_on(sink, policy)
}

/// Reopen the unfinished piece `path` (written as `path.partial` by [create] and cut off by a crash) to go on
/// writing it, see [TapeWriter::resume_on].
pub fn resume(
    path: &Path,
    options: WriterOptions,
) -> Result<(TapeWriter<FileSink>, Resumed), WriteError<FsError>> {
    let what = "opening the unfinished file";
    match FileSink::reopen(path).map_err(sink_err(what))? {
        Some(sink) => TapeWriter::resume_on(sink, options),
        None if path.exists() => Err(WriteError::Sink {
            what,
            error: FsError::Exists(path.to_path_buf()),
        }),
        None => Err(WriteError::Resume(ResumeError::NoPiece)),
    }
}

/// End the piece `writer` writes as [FinishState::Continued](ww_tape::FinishState::Continued) and start the next piece
/// of the chain in a new file at `next_path`, see [TapeWriter::cut_with].
pub fn cut(
    writer: TapeWriter<FileSink>,
    next_path: &Path,
) -> Result<(Finished, TapeWriter<FileSink>), WriteError<FsError>> {
    writer.cut_with(&mut Files::file(next_path), &0)
}
