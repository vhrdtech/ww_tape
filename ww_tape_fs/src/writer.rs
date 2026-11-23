//! Writers of files named by a path.

use std::path::Path;

use ww_tape::Definitions;
use ww_tape::sink::{TapeSink, sink_err};
use ww_tape::writer::{
    Closed, CutError, Resumed, SealError, Sealed, SyncPolicy, TapeWriter, WriteError, WriterOptions,
};
use ww_tape::{Integrity, TapeReader};

use crate::seal::InPlace;
use crate::sink::{FileSink, FileSource, Files, FsError, none_at};

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

/// Raise the closed file at `path` to [Integrity::Full] in place.
///
/// Its pieces are sealed in order, see [TapeWriter::seal_on], the first after the piece `after` when it follows a
/// piece sealed before. Gives each piece sealed: the last is what the file after it takes as `after`. A file whose
/// pieces are all of Full, given no `after`, is left as it is and gives none.
///
/// The file is locked as for a writer ([FsError::Locked] while one holds it), and every piece must be closed
/// ([SealError::NotClosed]) before a byte is written. Each block is written over itself, at the same length, and
/// each piece's head, which names its level, is written last, once the rest is synced. A crash or a refusal on the
/// way leaves a piece of its old level whose blocks hold valid CRCs and hashes, and a seal again raises it. With no
/// file there, the error is [FsError::NoPiece].
pub fn seal(path: &Path, after: Option<&Sealed>) -> Result<Vec<Sealed>, WriteError<FsError>> {
    let what = "opening a file to seal";
    let refused = |error| WriteError::Sink { what, error };
    if none_at(path) {
        return Err(refused(FsError::NoPiece(path.to_path_buf())));
    }
    let sink = FileSink::to_overwrite(path)
        .map_err(sink_err(what))?
        .ok_or_else(|| refused(FsError::NoPiece(path.to_path_buf())))?;
    let src = FileSource::open(path).map_err(sink_err(what))?;
    let reader = TapeReader::open(&src).map_err(WriteError::Read)?;
    let pieces = reader.pieces();
    if pieces.iter().any(|p| p.tail.is_none()) {
        return Err(WriteError::Seal(SealError::NotClosed));
    }
    if after.is_none() && pieces.iter().all(|p| p.integrity() == Integrity::Full) {
        return Ok(Vec::new());
    }
    let pieces: Vec<(u64, u64)> = pieces
        .iter()
        .map(|p| (p.start, p.end.saturating_sub(p.start)))
        .collect();
    let mut out = InPlace::new(sink);
    let mut sealed: Vec<Sealed> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        out.rebase(piece.0).map_err(sink_err("sealing a file"))?;
        let (s, sink) = TapeWriter::seal_on(&src, piece, out, sealed.last().or(after))?;
        out = sink;
        sealed.push(s);
    }
    out.finish().map_err(sink_err("sealing a file"))?;
    Ok(sealed)
}
