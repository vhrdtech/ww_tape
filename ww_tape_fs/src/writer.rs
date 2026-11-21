//! Writers of files named by a path.

use std::path::Path;

use ww_tape::Definitions;
use ww_tape::sink::sink_err;
use ww_tape::writer::{
    Closed, CutError, Resumed, SyncPolicy, TapeWriter, WriteError, WriterOptions,
};

use crate::sink::{FileSink, Files, FsError, none_at};

/// A new file at `path`, created exclusively under its final name; fails if `path` is there ([FileSink::create]).
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

/// Reopen the piece `path` to write on, see [TapeWriter::resume_on].
///
/// The resume reads from the file's bytes whether a crash cut it off, to go on writing it, or it is closed, to write
/// on in place after its last close record. With no file there, its directory missing too, the error is
/// [FsError::NoPiece] naming `path`.
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
        None => Err(refused(FsError::NoPiece(path.to_path_buf()))),
    }
}

/// End the piece `writer` writes by a cut and start the next piece of the chain in a new file at `next_path`.
///
/// See [TapeWriter::cut_with]: the close record names the next piece, whose head is followed by the definitions
/// `defs` holds of the tracks it carries. A cut refused before the piece ends hands the writer back in
/// [CutError::writer].
pub fn cut(
    writer: TapeWriter<FileSink>,
    defs: &Definitions,
    next_path: &Path,
) -> Result<(Closed, TapeWriter<FileSink>), CutError<FileSink>> {
    writer.cut_with(defs, &mut Files::file(next_path), &0)
}
